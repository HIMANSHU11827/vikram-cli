//! Feishu / Lark adapter — Open Platform API.
//!
//! Rust port of the wire half of hermes-agent `plugins/platforms/feishu/adapter.py`: the tenant
//! access token, `POST /im/v1/messages` (create and reply), `GET /im/v1/chats/{chat_id}`, the
//! `POST /im/v1/images` and `POST /im/v1/files` uploads, and the `im.message.receive_v1` webhook
//! payload. Everything hermes builds on `lark_oapi` is re-implemented here directly against the
//! Open Platform over [`crate::transport::Transport`], so the adapter stays testable with no
//! network, no app credentials and no SDK.
//!
//! Ported behaviour, with the hermes origin:
//! - Tenant access token: `POST /auth/v3/tenant_access_token/internal`, cached for `expire` seconds
//!   minus a 60 s safety margin and reused across sends (`lark_oapi`'s tenant-token cache).
//! - Outbound text: `msg_type: "text"` with a JSON-encoded `content` string, chunked at
//!   [`MAX_MESSAGE_LENGTH`] code points through [`truncate_message`] and delivered sequentially
//!   (`send`, `MAX_MESSAGE_LENGTH`, `truncate_message`).
//! - 8000-code-point cap and the delivery-result contract (`ok_split`, last id as `message_id`).
//! - Send failure taxonomy from Feishu's `code`/`msg` body plus the HTTP status, including the
//!   tenant-token codes and the "sending too fast" code (`_finalize_send_result`,
//!   `_response_error_result`).
//! - Inbound `im.message.receive_v1` normalization: text, image, file, audio (voice note) and
//!   video, with the resource key surfaced as a [`MessageEvent::media_urls`] entry
//!   (`normalize_feishu_message`, `_resolve_normalized_message_type`).
//! - Outbound media as two steps, like hermes. The bytes are uploaded first —
//!   `POST /im/v1/images` as `multipart/form-data` with `image_type=message` plus the `image` part,
//!   or `POST /im/v1/files` with `file_type`/`file_name` plus the `file` part — and only the key
//!   that comes back (`data.image_key` / `data.file_key`) is sent, as one message whose `content`
//!   JSON carries it and whose `msg_type` is `image` / `file` / `audio` / `media`
//!   (`send_image_file`, `_send_uploaded_file_message`, `_build_image_upload_body`,
//!   `_build_file_upload_body`, adapter.py:1917-1947 / 3688-3743 / 4064-4083). The `msg_type` and
//!   the upload `file_type` both come from [`resolve_file_routing`], the port of
//!   `_resolve_outbound_file_routing` (adapter.py:4091-4102). A failed upload ends the call with
//!   no message request at all; a failed send is reported as a failure, never as a delivery; the
//!   only success is the message Feishu acknowledged, with `data.message_id` as its id
//!   (`_finalize_send_result`, adapter.py:3821-3827).
//! - Outbound rich cards: `POST /im/v1/messages?receive_id_type=chat_id` with
//!   `msg_type: "interactive"` and the card as the JSON-encoded `content`, sent through the same
//!   message path as text ([`FeishuAdapter::send_card`]) — hermes' `_send_interactive_card`
//!   (adapter.py:1797-1814), whose own inbound reader accepts the `{"card": …}` wrapper this port
//!   sends (adapter.py:740).
//! - Reactions: `POST /im/v1/messages/{message_id}/reactions` carrying
//!   `{"reaction_type": {"emoji_type": …}}` to add, and
//!   `DELETE /im/v1/messages/{message_id}/reactions/{reaction_id}` to remove
//!   ([`FeishuAdapter::add_reaction`] / [`FeishuAdapter::remove_reaction`]) — hermes
//!   `_add_reaction` / `_remove_reaction` (adapter.py:2572-2594). Delete takes the opaque
//!   `reaction_id` that create returned, never the emoji, so the handle is cached against
//!   `(message_id, emoji)` where hermes caches it against `message_id` alone
//!   (`_pending_processing_reactions`, adapter.py:1347-1348): the trait is per-emoji, the
//!   reference only ever badges one emoji per message.
//!
//! Media fidelity notes:
//! - **Captions are not delivered.** hermes folds `caption` into a `post` message that embeds the
//!   media tag (`_send_uploaded_key` → `_build_media_post_payload`, adapter.py:3748-3761 /
//!   4085-4089). `post` payloads are not ported (see below), so this adapter sends the bare
//!   `image`/`file`/`audio`/`media` message and reports that message's delivery. The trait
//!   contract in [`crate::platform::OutboundMedia::caption`] leaves the follow-up text to the
//!   caller when a platform cannot carry the caption with the media.
//! - **`MediaSource::Url` is refused, with zero requests.** hermes downloads an http(s) URL itself
//!   under an SSRF guard and uploads the bytes (`send_image` → `_download_remote_image`,
//!   adapter.py:1953-1967 / 2779-2781; a download failure falls back to sending the URL as text
//!   through `super().send_image`). It never forwards a URL as an `image_key`. This port has no
//!   fetcher, so it tells the caller to supply bytes instead of pretending a URL went out.
//! - **No audio transcoding.** hermes re-encodes a voice note to Ogg/Opus with ffmpeg and, without
//!   ffmpeg, sends the original file as an attachment (`send_voice`, adapter.py:1867-1895). The
//!   routing table still applies here: `.ogg`/`.opus` bytes go out as an `audio` message, every
//!   other audio file goes out as a `file` attachment — the reference's own fallback.
//! - **No opus `duration` field.** hermes measures it in pure Python (`_get_audio_duration_ms`,
//!   adapter.py:3666-3684) and adds it only when it is non-zero; that Ogg granule scan is not
//!   ported, so an `audio` upload omits `duration` and Feishu reads the length from the file.
//!
//! Deliberately not ported (all need either the `lark_oapi` SDK, a long-lived connection, or
//! process state this stateless wire adapter does not own):
//! - the WebSocket client, its supervisor, reconnect ladders and the lark-WS multiplex shims —
//!   the host owns ingest and calls [`FeishuAdapter::parse_webhook`];
//! - the aiohttp webhook listener, its body limit, rate limiter and anomaly tracker — the host owns
//!   the listener and calls [`FeishuAdapter::verify_webhook`] / [`FeishuAdapter::parse_webhook`];
//! - inbound media download (`GET /im/v1/messages/{message_id}/resources/{file_key}`), rich `post`
//!   documents, inbound card (`interactive`) payload walking, approval buttons, the reaction
//!   *events* (`im.message.reaction.created_v1` / `.deleted_v1`, adapter.py:2177-2194) as inbound
//!   turns, `merge_forward` / `share_chat` payload walking, message recall, read receipts,
//!   drive-comment and meeting-invite events;
//! - dedup cache, `FEISHU_ALLOWED_USERS` admission, per-group policy rules, per-chat serial
//!   processing, sender-name resolution and burst batching — the runner owns admission;
//! - the reply→create fallback for withdrawn reply targets (codes 230011 / 231003) and the
//!   `post`→`text` retry: both need a second payload shape this port does not build;
//! - `edit_message`: the Open Platform's message-update endpoint is for app-sent cards, so an
//!   already-sent text message cannot be rewritten — the adapter says so instead of pretending.

use async_trait::async_trait;
use serde_json::{Value, json};
use std::collections::{HashMap, VecDeque};
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Mutex, MutexGuard};
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

use crate::event::{MessageEvent, MessageType, Platform, SessionSource, platform_names};
use crate::helpers::{codepoint_len, truncate_message};
use crate::platform::{
    ChatInfo, MediaKind, MediaSource, OutboundMedia, PlatformAdapter, SendResult,
};
use crate::transport::{HttpRequest, SharedTransport};

/// Feishu's Open Platform origin. Every request path hangs off this base.
pub const FEISHU_API_BASE: &str = "https://open.feishu.cn/open-apis";

/// Feishu's per-message text cap in code points (hermes `MAX_MESSAGE_LENGTH`); longer answers are
/// chunked by [`FeishuAdapter::send`].
pub const MAX_MESSAGE_LENGTH: usize = 8000;

/// The webhook event type this adapter turns into [`MessageEvent`]s.
pub const MESSAGE_RECEIVE_EVENT: &str = "im.message.receive_v1";

/// `msg_type` of an interactive (rich card) message — hermes
/// `_send_interactive_card` → `_feishu_send_with_retry(msg_type="interactive", …)`,
/// adapter.py:1797-1805.
pub const INTERACTIVE_MSG_TYPE: &str = "interactive";

/// `image_type` of an image uploaded for a chat message (hermes `_FEISHU_IMAGE_UPLOAD_TYPE`,
/// adapter.py:137, used by `send_image_file` at adapter.py:1930).
pub const IMAGE_UPLOAD_TYPE: &str = "message";

/// `file_type` of an otherwise unclassified upload (hermes `_FEISHU_FILE_UPLOAD_TYPE`,
/// adapter.py:138 — also the fallback in `_resolve_outbound_file_routing`).
pub const FILE_UPLOAD_TYPE: &str = "stream";

/// Largest image this adapter uploads to `POST /im/v1/images`; Feishu's documented cap for
/// `image_type=message` is 10 MB.
///
/// hermes has no local size gate for media — it hands the bytes straight to
/// `client.im.v1.image.create` / `client.im.v1.file.create` (adapter.py:1926-1934 /
/// 3701-3708) and lets Feishu reject an oversized upload with a non-zero `code`. Checking the
/// documented cap here refuses exactly what Feishu would refuse anyway, and keeps the bytes off
/// the wire.
pub const MAX_IMAGE_BYTES: usize = 10 * 1024 * 1024;

/// Largest file this adapter uploads to `POST /im/v1/files`; Feishu's documented cap is 30 MB.
/// See [`MAX_IMAGE_BYTES`] for why the gate is local.
pub const MAX_FILE_BYTES: usize = 30 * 1024 * 1024;

/// Extensions Feishu accepts as an `audio` (in-app voice) upload — hermes
/// `_FEISHU_OPUS_UPLOAD_EXTENSIONS` (adapter.py:139).
const OPUS_UPLOAD_EXTENSIONS: [&str; 2] = [".ogg", ".opus"];

/// Extensions Feishu accepts as a `media` (video) upload — hermes `_FEISHU_MEDIA_UPLOAD_EXTENSIONS`
/// (adapter.py:140).
const MEDIA_UPLOAD_EXTENSIONS: [&str; 4] = [".mp4", ".mov", ".avi", ".m4v"];

/// Extension → `file_type` for the documents Feishu gives a typed bubble — hermes
/// `_FEISHU_DOC_UPLOAD_TYPES` (adapter.py:141-143).
const DOC_UPLOAD_TYPES: [(&str, &str); 7] = [
    (".pdf", "pdf"),
    (".doc", "doc"),
    (".docx", "doc"),
    (".xls", "xls"),
    (".xlsx", "xls"),
    (".ppt", "ppt"),
    (".pptx", "ppt"),
];

/// Content type an upload part declares when the caller supplied no MIME type.
const DEFAULT_UPLOAD_MIME: &str = "application/octet-stream";

/// The Feishu emoji key hermes uses for its "working on it" badge — `_FEISHU_REACTION_IN_PROGRESS`
/// (adapter.py:196). Feishu reactions are prominent badges rather than Discord/Telegram's small
/// footer emoji, so the reference marks start and failure only (the reply itself is the success
/// signal).
pub const REACTION_IN_PROGRESS: &str = "Typing";

/// The Feishu emoji key hermes uses for its failure badge — `_FEISHU_REACTION_FAILURE`
/// (adapter.py:197).
pub const REACTION_FAILURE: &str = "CrossMark";

/// Bound on the `(message_id, emoji) → reaction_id` cache — hermes
/// `_FEISHU_PROCESSING_REACTION_CACHE_SIZE` (adapter.py:198-201). Happy-path entries drain on a
/// successful delete; the cap is a safeguard against the growth delete failures can cause, not a
/// capacity plan.
pub const REACTION_CACHE_CAPACITY: usize = 1024;

/// Counter behind [`fresh_boundary`]; keeps two uploads in the same nanosecond apart.
static MULTIPART_SEQUENCE: AtomicU64 = AtomicU64::new(0);

/// A tenant token is renewed this long before Feishu says it expires, so an in-flight send never
/// races the deadline (hermes relies on the SDK cache, which applies the same margin).
const TOKEN_SAFETY_MARGIN_SECS: i64 = 60;

/// Feishu codes meaning "the tenant access token is missing, expired or invalid".
const TOKEN_ERROR_CODES: [i64; 2] = [99_991_663, 99_991_661];

/// Feishu codes for a chat or user that does not exist (or that the bot cannot see).
const NOT_FOUND_ERROR_CODES: [i64; 2] = [230_002, 230_001];

/// Feishu's "you are sending too fast" code (`frequency limit`).
const FREQUENCY_LIMITED_CODE: i64 = 230_020;

/// One tenant access token plus the instant it stops being usable.
#[derive(Debug, Clone)]
pub struct CachedToken {
    /// The `tenant_access_token` Feishu issued.
    pub value: String,
    /// When the token must be replaced: issue time + `expire` − [`TOKEN_SAFETY_MARGIN_SECS`].
    pub expires_at: Instant,
}

/// Feishu / Lark Open Platform adapter.
///
/// Holds one shared [`crate::transport::Transport`], the app credentials, the tenant-token cache and
/// the reaction handles. Both caches are [`Mutex`] slots so several tasks can share one adapter; a
/// mutex is never held across an `await`.
pub struct FeishuAdapter {
    transport: SharedTransport,
    app_id: String,
    app_secret: String,
    api_base: String,
    token: Mutex<Option<CachedToken>>,
    /// `(message_id, emoji)` → the opaque `reaction_id` Feishu returned, which is the only thing
    /// `DELETE …/reactions/{reaction_id}` accepts (hermes `_pending_processing_reactions`,
    /// adapter.py:1347-1348).
    reactions: Mutex<ReactionCache>,
    /// Webhook verification token; `None` means verification fails closed (see
    /// [`FeishuAdapter::verify_webhook`]).
    verification_token: Option<String>,
}

impl FeishuAdapter {
    /// Build an adapter for `app_id`/`app_secret` over `transport`, talking to [`FEISHU_API_BASE`].
    pub fn new(
        transport: SharedTransport,
        app_id: impl Into<String>,
        app_secret: impl Into<String>,
    ) -> Self {
        Self {
            transport,
            app_id: app_id.into(),
            app_secret: app_secret.into(),
            api_base: FEISHU_API_BASE.to_string(),
            token: Mutex::new(None),
            reactions: Mutex::new(ReactionCache::default()),
            verification_token: None,
        }
    }

    /// Point the adapter at a mirror or a test double; a trailing `/` is dropped.
    pub fn with_api_base(mut self, base: impl Into<String>) -> Self {
        self.api_base = base.into().trim_end_matches('/').to_string();
        self
    }

    /// Configure the Open Platform verification token webhook payloads must carry.
    ///
    /// Without it [`FeishuAdapter::verify_webhook`] rejects everything (fail closed); an empty
    /// token counts as unconfigured for the same reason.
    pub fn with_verification_token(mut self, token: impl Into<String>) -> Self {
        let token = token.into().trim().to_string();
        self.verification_token = if token.is_empty() { None } else { Some(token) };
        self
    }

    /// The current tenant access token: the cached one while it is still live, otherwise a fresh
    /// one from `POST /auth/v3/tenant_access_token/internal`.
    ///
    /// Single-flight is not attempted (two concurrent misses may both fetch); the [`Mutex`] only
    /// guards the cache slot, which is what keeps later calls from re-fetching.
    pub async fn tenant_token(&self) -> Result<String, String> {
        self.acquire_token()
            .await
            .map_err(|failure| failure.message)
    }

    /// Accept a webhook delivery only when it carries the configured verification token.
    ///
    /// Feishu puts the token in `header.token`; older payloads carry a top-level `token`. The check
    /// is constant-time and **fails closed**: no configured token ⇒ `false`, and a payload without
    /// a token ⇒ `false`. That is deliberate — an unauthenticated webhook could otherwise inject
    /// arbitrary prompt text, so the host must configure a token before forwarding payloads here.
    pub fn verify_webhook(&self, body: &Value) -> bool {
        let Some(expected) = self.verification_token.as_deref() else {
            return false;
        };
        let provided = body
            .pointer("/header/token")
            .and_then(Value::as_str)
            .or_else(|| body.get("token").and_then(Value::as_str));
        match provided {
            Some(provided) => constant_time_eq(provided.trim().as_bytes(), expected.as_bytes()),
            None => false,
        }
    }

    /// Normalize one webhook delivery into events.
    ///
    /// Only `im.message.receive_v1` payloads produce events: `url_verification` handshakes, card
    /// actions, member churn, recalled messages and other event types are skipped (an empty
    /// vector, not an error). Message types this port has no shape for (`post`, `interactive`,
    /// `merge_forward`, `share_chat`, `sticker`) and message events missing `chat_id`/`message_id`
    /// are skipped too. Invalid JSON is an `Err`, because that means the host is not forwarding
    /// Feishu payloads at all.
    pub fn parse_webhook(&self, body: &str) -> Result<Vec<MessageEvent>, String> {
        let root: Value =
            serde_json::from_str(body).map_err(|err| format!("invalid json: {err}"))?;
        let event_type = root
            .pointer("/header/event_type")
            .and_then(Value::as_str)
            .unwrap_or_default();
        if event_type != MESSAGE_RECEIVE_EVENT {
            return Ok(Vec::new());
        }
        let Some(event) = root.get("event") else {
            return Ok(Vec::new());
        };
        let Some(message) = event.get("message") else {
            return Ok(Vec::new());
        };
        match parse_message(&root, event, message) {
            Some(event) => Ok(vec![event]),
            None => Ok(Vec::new()),
        }
    }

    /// `{base}/auth/v3/tenant_access_token/internal`.
    fn token_url(&self) -> String {
        format!("{}/auth/v3/tenant_access_token/internal", self.api_base)
    }

    /// `{base}/im/v1/messages?receive_id_type=chat_id` — the create-message endpoint.
    fn messages_url(&self) -> String {
        format!("{}/im/v1/messages?receive_id_type=chat_id", self.api_base)
    }

    /// `{base}/im/v1/messages/{message_id}/reply`.
    fn reply_url(&self, message_id: &str) -> String {
        format!(
            "{}/im/v1/messages/{}/reply",
            self.api_base,
            encode_path_segment(message_id)
        )
    }

    /// `{base}/im/v1/images` — the image-upload endpoint (hermes `client.im.v1.image.create`,
    /// adapter.py:1931-1932).
    fn images_url(&self) -> String {
        format!("{}/im/v1/images", self.api_base)
    }

    /// `{base}/im/v1/files` — the file-upload endpoint (hermes `client.im.v1.file.create`,
    /// adapter.py:3707-3708).
    fn files_url(&self) -> String {
        format!("{}/im/v1/files", self.api_base)
    }

    /// `{base}/im/v1/messages/{message_id}/reactions` — the add-reaction endpoint (hermes
    /// `CreateMessageReactionRequest`, `_add_reaction`, adapter.py:2572-2583).
    ///
    /// The endpoint is message-scoped: Feishu takes no chat id here, so [`FeishuAdapter::add_reaction`]
    /// validates the caller's chat id and then does not transmit it.
    fn reactions_url(&self, message_id: &str) -> String {
        format!(
            "{}/im/v1/messages/{}/reactions",
            self.api_base,
            encode_path_segment(message_id)
        )
    }

    /// `{base}/im/v1/messages/{message_id}/reactions/{reaction_id}` — the delete-reaction endpoint
    /// (hermes `DeleteMessageReactionRequest`, `_remove_reaction`, adapter.py:2585-2594).
    fn reaction_url(&self, message_id: &str, reaction_id: &str) -> String {
        format!(
            "{}/im/v1/messages/{}/reactions/{}",
            self.api_base,
            encode_path_segment(message_id),
            encode_path_segment(reaction_id)
        )
    }

    /// `{base}/im/v1/chats/{chat_id}`.
    fn chat_url(&self, chat_id: &str) -> String {
        format!(
            "{}/im/v1/chats/{}",
            self.api_base,
            encode_path_segment(chat_id)
        )
    }

    /// Never-panicking token-cache guard: a poisoned mutex still yields the slot.
    fn token_slot(&self) -> MutexGuard<'_, Option<CachedToken>> {
        match self.token.lock() {
            Ok(guard) => guard,
            Err(poisoned) => poisoned.into_inner(),
        }
    }

    /// The cached token while it has not reached `expires_at`.
    fn live_cached_token(&self) -> Option<String> {
        let slot = self.token_slot();
        let cached = slot.as_ref()?;
        if Instant::now() < cached.expires_at {
            Some(cached.value.clone())
        } else {
            None
        }
    }

    /// Cache `value` for `ttl_secs`; a zero TTL stores nothing, so a short-lived token simply gets
    /// re-fetched next time instead of being handed out already stale.
    fn store_token(&self, value: &str, ttl_secs: u64) {
        let Some(expires_at) = Instant::now().checked_add(Duration::from_secs(ttl_secs)) else {
            return;
        };
        *self.token_slot() = Some(CachedToken {
            value: value.to_string(),
            expires_at,
        });
    }

    /// Drop the cached token; the next call fetches a new one.
    fn invalidate_token(&self) {
        *self.token_slot() = None;
    }

    /// The cached token or a freshly fetched one, with the failure taxonomy intact.
    async fn acquire_token(&self) -> Result<String, Failure> {
        if let Some(token) = self.live_cached_token() {
            return Ok(token);
        }
        self.fetch_tenant_token().await
    }

    /// `POST /auth/v3/tenant_access_token/internal`, caching the token on `code == 0`.
    async fn fetch_tenant_token(&self) -> Result<String, Failure> {
        let payload = json!({ "app_id": self.app_id, "app_secret": self.app_secret });
        let request = HttpRequest::json(self.token_url(), &payload);
        let response = self
            .transport
            .execute(request)
            .await
            .map_err(|err| Failure::transient(format!("feishu token request failed: {err}")))?;
        let parsed = response.json().unwrap_or(Value::Null);
        if !response.ok() || body_code(&parsed) != Some(0) {
            return Err(credentials_failure(response.status, &parsed));
        }
        let token = parsed
            .get("tenant_access_token")
            .and_then(Value::as_str)
            .map(str::trim)
            .filter(|token| !token.is_empty());
        let Some(token) = token else {
            return Err(Failure::unknown(
                "feishu token response carried no tenant_access_token",
            ));
        };
        let expire = parsed.get("expire").and_then(Value::as_i64).unwrap_or(0);
        let ttl_secs = expire.saturating_sub(TOKEN_SAFETY_MARGIN_SECS);
        if ttl_secs > 0 {
            self.store_token(token, ttl_secs as u64);
        }
        Ok(token.to_string())
    }

    /// One message through the reply or the create endpoint, returning its `data.message_id`.
    ///
    /// `content` is the **JSON string** Feishu wants for `msg_type`'s body — `{"text": …}` for a
    /// text chunk, `{"image_key": …}` or `{"file_key": …}` for media (hermes
    /// `_build_create_message_body` / `_build_reply_message_body`, adapter.py:4032-4062). A reply
    /// targets `/im/v1/messages/{id}/reply` and carries no `receive_id`, exactly like
    /// `_send_raw_message` (adapter.py:3779-3801).
    ///
    /// hermes also sends a `uuid` idempotency key and `reply_in_thread`; neither is set here, for
    /// the same reason `send` does not set them: thread routing is host state this stateless wire
    /// adapter does not own.
    async fn send_payload(
        &self,
        chat_id: &str,
        msg_type: &str,
        content: &str,
        reply_to: Option<&str>,
    ) -> Result<String, Failure> {
        let token = self.acquire_token().await?;
        let request = match reply_to {
            Some(message_id) => {
                let payload = json!({ "msg_type": msg_type, "content": content });
                HttpRequest::json(self.reply_url(message_id), &payload).bearer(&token)
            }
            None => {
                let payload = json!({
                    "receive_id": chat_id,
                    "msg_type": msg_type,
                    "content": content,
                });
                HttpRequest::json(self.messages_url(), &payload).bearer(&token)
            }
        };
        let response = self
            .transport
            .execute(request)
            .await
            .map_err(|err| Failure::transient(format!("feishu send transport error: {err}")))?;
        let parsed = response.json().unwrap_or(Value::Null);
        if !response.ok() || !matches!(body_code(&parsed), None | Some(0)) {
            let failure = classify(response.status, &parsed, response.retry_after);
            if failure.token_invalid {
                self.invalidate_token();
            }
            return Err(failure);
        }
        match parsed
            .pointer("/data/message_id")
            .and_then(Value::as_str)
            .map(str::trim)
            .filter(|id| !id.is_empty())
        {
            Some(message_id) => Ok(message_id.to_string()),
            None => Err(Failure::unknown(
                "feishu send response carried no data.message_id",
            )),
        }
    }

    /// One text chunk: `msg_type: "text"` with `{"text": chunk}` as the JSON-encoded content.
    async fn send_chunk(
        &self,
        chat_id: &str,
        chunk: &str,
        reply_to: Option<&str>,
    ) -> Result<String, Failure> {
        let content = json!({ "text": chunk }).to_string();
        self.send_payload(chat_id, "text", &content, reply_to).await
    }

    /// Never-panicking reaction-cache guard: a poisoned mutex still yields the cache.
    fn reaction_slot(&self) -> MutexGuard<'_, ReactionCache> {
        match self.reactions.lock() {
            Ok(guard) => guard,
            Err(poisoned) => poisoned.into_inner(),
        }
    }

    /// The cached reaction handle for `(message_id, emoji)`, if one was recorded.
    fn cached_reaction_id(&self, message_id: &str, emoji: &str) -> Option<String> {
        self.reaction_slot().get(message_id, emoji)
    }

    /// Record `reaction_id` for `(message_id, emoji)`, replacing an older handle for the same pair.
    fn remember_reaction_id(&self, message_id: &str, emoji: &str, reaction_id: &str) {
        self.reaction_slot().store(message_id, emoji, reaction_id);
    }

    /// Drop the handle for `(message_id, emoji)`; only a confirmed delete may call this.
    fn forget_reaction_id(&self, message_id: &str, emoji: &str) {
        self.reaction_slot().remove(message_id, emoji);
    }

    /// Perform one authenticated reaction call and return the parsed success envelope.
    ///
    /// Classification is [`classify`], the taxonomy the send and upload paths already use, so a
    /// caller sees one error model across every Feishu write: HTTP 429 (or code 230020) is
    /// `rate_limited` with `Retry-After` honoured, a tenant-token code is `forbidden` and drops the
    /// cached token, 404/230002/230001 are `not_found`, other 4xx are `bad_format`, 5xx are
    /// `transient` (`retryable`), and a transport error is `transient`. A 2xx whose body still
    /// carries a non-zero `code` is a failure too — the envelope, not the status, decides
    /// (`_response_error_result`, adapter.py:3812-3819), exactly as hermes' `_reaction_call` only
    /// treats a response as success when `_response_succeeded` holds (adapter.py:2558-2570).
    async fn reaction_request(&self, request: HttpRequest, step: &str) -> Result<Value, Failure> {
        let token = self.acquire_token().await?;
        let response = self
            .transport
            .execute(request.bearer(&token))
            .await
            .map_err(|err| Failure::transient(format!("feishu {step} transport error: {err}")))?;
        let parsed = response.json().unwrap_or(Value::Null);
        if !response.ok() || !matches!(body_code(&parsed), None | Some(0)) {
            let failure = classify(response.status, &parsed, response.retry_after);
            if failure.token_invalid {
                self.invalidate_token();
            }
            return Err(failure);
        }
        Ok(parsed)
    }

    /// `POST /im/v1/images` as `multipart/form-data` (`image_type=message` + the `image` part),
    /// returning `data.image_key`.
    ///
    /// Mirrors hermes' `send_image_file` (adapter.py:1926-1944): `_build_image_upload_body` fills
    /// `image_type=_FEISHU_IMAGE_UPLOAD_TYPE` and the file part, and the response is asked for
    /// `image_key` (`_extract_response_field`, adapter.py:3807-3810). A body without that key is a
    /// failure, not an empty upload (hermes: "Feishu image upload missing image_key").
    async fn upload_image(
        &self,
        filename: &str,
        content_type: &str,
        bytes: &[u8],
    ) -> Result<String, Failure> {
        let fields = [("image_type", IMAGE_UPLOAD_TYPE.to_string())];
        let file = FilePart {
            field: "image",
            filename,
            content_type,
            bytes,
        };
        let parsed = self
            .post_multipart(
                self.images_url(),
                multipart_body(&fields, file),
                "image upload",
            )
            .await?;
        match key_field(&parsed, "image_key") {
            Some(image_key) => Ok(image_key),
            None => Err(Failure::unknown(
                "feishu image upload response carried no data.image_key",
            )),
        }
    }

    /// `POST /im/v1/files` as `multipart/form-data` (`file_type`, `file_name` + the `file` part),
    /// returning `data.file_key`.
    ///
    /// Mirrors hermes' `_send_uploaded_file_message` (adapter.py:3701-3714) with
    /// `_build_file_upload_body` (adapter.py:4073-4079): `file_type` and the display `file_name`
    /// come from [`resolve_file_routing`], the bytes ride in `file`. The opus `duration` field is
    /// not sent — see the module note. A body without `file_key` is a failure (hermes: "Feishu file
    /// upload missing file_key").
    async fn upload_file(
        &self,
        file_type: &str,
        file_name: &str,
        content_type: &str,
        bytes: &[u8],
    ) -> Result<String, Failure> {
        let fields = [
            ("file_type", file_type.to_string()),
            ("file_name", file_name.to_string()),
        ];
        let file = FilePart {
            field: "file",
            filename: file_name,
            content_type,
            bytes,
        };
        let parsed = self
            .post_multipart(
                self.files_url(),
                multipart_body(&fields, file),
                "file upload",
            )
            .await?;
        match key_field(&parsed, "file_key") {
            Some(file_key) => Ok(file_key),
            None => Err(Failure::unknown(
                "feishu file upload response carried no data.file_key",
            )),
        }
    }

    /// Perform one authenticated multipart upload and return the parsed success envelope.
    ///
    /// Upload failures are classified by the same [`classify`] the message steps use, so a caller
    /// sees one taxonomy across both halves of a media send: HTTP 429 (or code 230020) is
    /// `rate_limited` with `Retry-After` honoured, a token code is `forbidden` and drops the cached
    /// token, 404/230002/230001 are `not_found`, other 4xx are `bad_format`, 5xx are `transient`,
    /// and a transport error is `transient`. A 2xx whose body still carries a non-zero `code` is a
    /// failure too — the envelope, not the status, decides (`_response_error_result`,
    /// adapter.py:3812-3819).
    async fn post_multipart(
        &self,
        url: String,
        multipart: MultipartBody,
        step: &str,
    ) -> Result<Value, Failure> {
        let token = self.acquire_token().await?;
        let request = HttpRequest::post(url)
            .bearer(&token)
            .header(
                "Content-Type",
                format!("multipart/form-data; boundary={}", multipart.boundary),
            )
            .body(multipart.body);
        let response =
            self.transport.execute(request).await.map_err(|err| {
                Failure::transient(format!("feishu {step} transport error: {err}"))
            })?;
        let parsed = response.json().unwrap_or(Value::Null);
        if !response.ok() || !matches!(body_code(&parsed), None | Some(0)) {
            let failure = classify(response.status, &parsed, response.retry_after);
            if failure.token_invalid {
                self.invalidate_token();
            }
            return Err(failure);
        }
        Ok(parsed)
    }

    /// Upload `bytes` for `media` and send the one message that references them.
    ///
    /// This is hermes' two-step media path: `send_image_file` uploads and then sends
    /// `{"image_key": …}` as `msg_type: "image"` (adapter.py:1930-1944), while
    /// `_send_uploaded_file_message` uploads and then sends `{"file_key": …}` as the `msg_type`
    /// [`resolve_file_routing`] picked — `file`, `audio` or `media` (adapter.py:3698-3743). A
    /// failed upload returns before any message request; a failed send is returned as that failure.
    async fn send_media_bytes(
        &self,
        chat_id: &str,
        media: &OutboundMedia,
        bytes: &[u8],
        reply_to: Option<&str>,
    ) -> Result<String, Failure> {
        let filename = media_filename(media);
        let content_type = upload_content_type(media);
        match media.kind {
            MediaKind::Image => {
                let image_key = self.upload_image(&filename, &content_type, bytes).await?;
                let content = json!({ "image_key": image_key }).to_string();
                self.send_payload(chat_id, "image", &content, reply_to)
                    .await
            }
            kind => {
                let (file_type, msg_type) =
                    resolve_file_routing(&filename, requested_msg_type(kind));
                let file_key = self
                    .upload_file(file_type, &filename, &content_type, bytes)
                    .await?;
                let content = json!({ "file_key": file_key }).to_string();
                self.send_payload(chat_id, msg_type, &content, reply_to)
                    .await
            }
        }
    }
}

#[async_trait]
impl PlatformAdapter for FeishuAdapter {
    /// Fetch a tenant token. True only when `/auth/v3/tenant_access_token/internal` answers
    /// `code == 0` with a token, which is the only proof the app credentials are live. There is no
    /// long-lived connection to re-establish, so `is_reconnect` needs no different handling.
    async fn connect(&mut self, _is_reconnect: bool) -> bool {
        self.acquire_token().await.is_ok()
    }

    /// Forget the cached tenant token. Nothing else is held client-side: the host owns ingest
    /// (webhook listener or WebSocket client), so there is no session to tear down.
    async fn disconnect(&mut self) {
        self.invalidate_token();
    }

    /// Trim, chunk to [`MAX_MESSAGE_LENGTH`] code points and deliver the chunks in order.
    ///
    /// When `reply_to` is set every chunk is posted to `/im/v1/messages/{id}/reply` instead of the
    /// create endpoint — the same shape hermes uses, where the reply decision sits below the
    /// chunking loop, so a split answer stays threaded under the message it answers.
    ///
    /// Blank content is refused with `bad_format` before any request: a text message with no text
    /// is not something Feishu can deliver, and silently succeeding would hide a broken reply.
    async fn send(
        &self,
        chat_id: &str,
        content: &str,
        reply_to: Option<&str>,
        _metadata: Option<Value>,
    ) -> SendResult {
        let trimmed = content.trim();
        if trimmed.is_empty() {
            return SendResult::failed(
                "feishu cannot send an empty text message",
                Some("bad_format"),
            );
        }
        let chunks = truncate_message(trimmed, MAX_MESSAGE_LENGTH, codepoint_len);
        if chunks.is_empty() {
            return SendResult::failed(
                "feishu cannot send an empty text message",
                Some("bad_format"),
            );
        }
        let mut delivered: Vec<String> = Vec::with_capacity(chunks.len());
        for chunk in &chunks {
            match self.send_chunk(chat_id, chunk, reply_to).await {
                Ok(message_id) => delivered.push(message_id),
                Err(failure) => {
                    // Report the ids that already landed so a retry sends only the tail.
                    let mut result = failure.into_send_result();
                    result.continuation_message_ids = delivered;
                    return result;
                }
            }
        }
        match delivered.last() {
            Some(last) => {
                let last = last.clone();
                SendResult::ok_split(last, delivered)
            }
            None => SendResult::failed("feishu delivered no chunks", Some("unknown")),
        }
    }

    /// `GET /im/v1/chats/{chat_id}`, reporting `data.name`.
    ///
    /// Feishu chats are groups; `data.chat_mode` (or, on older payloads, `data.chat_type`)
    /// `"p2p"` is the one-to-one chat with the bot and maps to `"dm"`. Any failure degrades to
    /// `{name: chat_id, chat_type: "group"}` so callers never lose routing.
    async fn get_chat_info(&self, chat_id: &str) -> ChatInfo {
        let fallback = ChatInfo {
            name: chat_id.to_string(),
            chat_type: "group".to_string(),
        };
        let Ok(token) = self.acquire_token().await else {
            return fallback;
        };
        let request = HttpRequest::get(self.chat_url(chat_id)).bearer(&token);
        let response = match self.transport.execute(request).await {
            Ok(response) => response,
            Err(_) => return fallback,
        };
        let parsed = response.json().unwrap_or(Value::Null);
        if !response.ok() || body_code(&parsed) != Some(0) {
            if body_code(&parsed).is_some_and(|code| TOKEN_ERROR_CODES.contains(&code)) {
                self.invalidate_token();
            }
            return fallback;
        }
        let data = parsed.get("data");
        let name = data
            .and_then(|data| data.get("name"))
            .and_then(Value::as_str)
            .map(str::trim)
            .filter(|name| !name.is_empty())
            .map(str::to_string)
            .unwrap_or_else(|| chat_id.to_string());
        ChatInfo {
            name,
            chat_type: if is_p2p_chat(data) { "dm" } else { "group" }.to_string(),
        }
    }

    /// Whether [`send_media`](Self::send_media) is implemented here: it is (upload + message), so
    /// the router may hand attachments straight to this adapter instead of the text fallback.
    fn supports_media(&self) -> bool {
        true
    }

    /// Upload one attachment, then send the message that references the uploaded key.
    ///
    /// The two steps mirror hermes: `POST /im/v1/images` (`image_type=message` + the `image` part)
    /// for an image, `POST /im/v1/files` (`file_type`, `file_name` + the `file` part) for
    /// everything else, then one `POST /im/v1/messages?receive_id_type=chat_id` — or the reply
    /// endpoint when `reply_to` is set — whose `content` is the JSON string `{"image_key": …}` /
    /// `{"file_key": …}` and whose `msg_type` is `image`, `file`, `audio` or `media`
    /// ([`resolve_file_routing`]) (adapter.py:1917-1947 / 3688-3743).
    ///
    /// Failures are never reported as deliveries:
    /// - a local refusal (empty bytes, over [`MAX_IMAGE_BYTES`] / [`MAX_FILE_BYTES`],
    ///   [`MediaSource::Url`]) makes no request at all;
    /// - an upload failure (`code != 0`, a 4xx/5xx status, a transport error, or a 2xx with no
    ///   `image_key`/`file_key`) ends the call with that failure and **no** message request;
    /// - a send failure after a successful upload is surfaced as a failure — the media is uploaded
    ///   but never delivered, and `message_id` stays `None`;
    /// - success is only the message Feishu acknowledged, with `data.message_id` as its id.
    ///
    /// A `caption` is not delivered: see the module note — hermes folds it into an unported `post`
    /// payload, so this adapter sends the bare key message and the caller owns the follow-up text.
    async fn send_media(
        &self,
        chat_id: &str,
        media: &OutboundMedia,
        reply_to: Option<&str>,
    ) -> SendResult {
        let bytes = match &media.source {
            MediaSource::Bytes(bytes) => bytes,
            MediaSource::Url(url) => return url_refusal(url),
        };
        if bytes.is_empty() {
            // Nothing to upload: a 0-byte attachment is a caller bug, not a platform problem.
            return SendResult::failed("feishu cannot upload empty media", Some("bad_format"));
        }
        if let Some(failure) = size_failure(media.kind, bytes.len()) {
            return failure;
        }
        match self.send_media_bytes(chat_id, media, bytes, reply_to).await {
            Ok(message_id) => SendResult::ok(message_id),
            Err(failure) => failure.into_send_result(),
        }
    }

    /// Feishu renders fenced code blocks.
    fn supports_code_blocks(&self) -> bool {
        true
    }

    /// Rich cards are implemented: the Open Platform sends an interactive card as an
    /// `msg_type: "interactive"` message, the lane hermes uses for its button cards
    /// (`_send_interactive_card`, adapter.py:1797-1814).
    fn supports_cards(&self) -> bool {
        true
    }

    /// Send one interactive card.
    ///
    /// `card` is the interactive card object itself — the shape hermes builds in `_card`
    /// (`config` / `header` / `elements`, adapter.py:1265-1274). It travels the adapter's existing
    /// message path, [`send_payload`](Self::send_payload): one `POST /im/v1/messages` with
    /// `receive_id_type=chat_id` (the endpoint [`messages_url`](Self::messages_url) already spells),
    /// `receive_id` = the chat, `msg_type: "interactive"` and `content` the JSON *string*
    /// `{"card": <card>}`.
    ///
    /// Why that content shape: hermes hands the card to the SDK as the message content and
    /// `json.dumps` it (`_send_interactive_card`, adapter.py:1802-1805), and its own inbound
    /// reader treats a card payload as wrapped when the wrapper is there —
    /// `card_payload = payload.get("card") if isinstance(payload.get("card"), dict) else payload`
    /// (adapter.py:740) — so `{"card": …}` is the documented interactive envelope and the shape the
    /// reference reads back. No `uuid` idempotency key and no `reply_in_thread` are sent, for the
    /// same reason [`send`](Self::send) sends neither: thread routing is host state this stateless
    /// wire adapter does not own (compare `_build_create_message_body`, adapter.py:4055-4061).
    ///
    /// Refused before any request, with `bad_format` and zero requests: a blank chat id, and a
    /// `card` that is not a JSON object (a string, a number, an array, `null`). The card's internal
    /// schema is not re-validated here — Feishu answers a malformed card with a non-zero `code`,
    /// which [`classify`] turns into a typed failure — and a reply is never attempted, because
    /// `send_card` is handed no `reply_to` (hermes passes `reply_to=None` on the same send).
    ///
    /// Success is only the message Feishu acknowledged, with `data.message_id` as its id
    /// (`_finalize_send_result`, adapter.py:3821-3827); `code != 0`, a 4xx/5xx status or a transport
    /// error is a typed failure, never a claimed delivery.
    async fn send_card(&self, chat_id: &str, card: &Value) -> SendResult {
        let chat_id = chat_id.trim();
        if chat_id.is_empty() {
            return SendResult::failed("feishu send card: chat id is empty", Some("bad_format"));
        }
        if !card.is_object() {
            return SendResult::failed(
                format!(
                    "feishu send card: a card must be a JSON object (an interactive card carrying \
                     `config` / `header` / `elements`), got {}",
                    json_type_name(card)
                ),
                Some("bad_format"),
            );
        }
        let content = json!({ "card": card }).to_string();
        match self
            .send_payload(chat_id, INTERACTIVE_MSG_TYPE, &content, None)
            .await
        {
            Ok(message_id) => SendResult::ok(message_id),
            Err(failure) => failure.into_send_result(),
        }
    }

    /// Feishu has a per-message reaction API, so [`add_reaction`](Self::add_reaction) and
    /// [`remove_reaction`](Self::remove_reaction) are implemented.
    fn supports_reactions(&self) -> bool {
        true
    }

    /// Add `emoji` to `message_id` with `POST /im/v1/messages/{message_id}/reactions` carrying
    /// `{"reaction_type": {"emoji_type": …}}`.
    ///
    /// Value mapping is the reference's, unchanged: hermes' `_add_reaction` puts its `emoji_type`
    /// argument straight into `CreateMessageReactionRequestBody…reaction_type({"emoji_type":
    /// emoji_type})` (adapter.py:2572-2583) and every caller hands it a Feishu key — `Typing` and
    /// `CrossMark` (adapter.py:196-197, [`REACTION_IN_PROGRESS`] / [`REACTION_FAILURE`]). hermes
    /// has no glyph table anywhere, so none is invented here: a Unicode emoji is passed through
    /// trimmed, in the same field a key would take, and left for Feishu to accept or reject. A
    /// rejection is a failure — never a claimed reaction.
    ///
    /// Refused with **zero** requests (no token fetch either) and no cache change: a blank
    /// `chat_id`, `message_id` or `emoji`, which mirrors hermes' guard `if not message_id or not
    /// emoji_type: return None` (adapter.py:2574) plus this port's own blank-chat-id refusal. The
    /// endpoint itself is message-scoped — Feishu takes no chat id here — so the chat id is
    /// validated and then not transmitted.
    ///
    /// Success is strict: `code == 0` **and** a non-blank `data.reaction_id`, because that opaque id
    /// is the only handle `DELETE …/reactions/{reaction_id}` accepts (hermes reads
    /// `getattr(data, "reaction_id", None)`, adapter.py:2583). A code-0 answer without one cannot be
    /// undone, so it is reported as `false` instead of a reaction. On success the id is cached
    /// against `(message_id, emoji)` for [`remove_reaction`](Self::remove_reaction); on any failure
    /// (`code != 0`, 4xx/5xx, a transport error) nothing is cached.
    ///
    /// A repeat add for the same pair re-sends and replaces the cached handle rather than skipping:
    /// hermes' skip-if-cached guard lives in its processing lifecycle (`if … message_id in
    /// self._pending_processing_reactions: return`, adapter.py:2598), which is caller state this
    /// wire adapter does not own, and answering `true` from a stale handle would claim a badge that
    /// may no longer be on the message.
    async fn add_reaction(&self, chat_id: &str, message_id: &str, emoji: &str) -> bool {
        if chat_id.trim().is_empty() {
            return false;
        }
        let message_id = message_id.trim();
        if message_id.is_empty() {
            return false;
        }
        let Some(emoji_type) = reaction_emoji_type(emoji) else {
            return false;
        };
        let payload = json!({ "reaction_type": { "emoji_type": emoji_type } });
        let request = HttpRequest::json(self.reactions_url(message_id), &payload);
        let Ok(parsed) = self.reaction_request(request, "reaction add").await else {
            return false;
        };
        // Both halves are required: an envelope without `code: 0` or without the id is not a
        // reaction this adapter can claim, or later remove. `key_field` reads `data.reaction_id`
        // the way hermes' `getattr(data, "reaction_id", None)` does (adapter.py:2583).
        match (body_code(&parsed), key_field(&parsed, "reaction_id")) {
            (Some(0), Some(reaction_id)) => {
                self.remember_reaction_id(message_id, &emoji_type, &reaction_id);
                true
            }
            _ => false,
        }
    }

    /// Remove the reaction [`add_reaction`](Self::add_reaction) added for `emoji` on `message_id`
    /// with `DELETE /im/v1/messages/{message_id}/reactions/{reaction_id}`.
    ///
    /// Feishu deletes by the opaque `reaction_id`, never by emoji: hermes builds
    /// `DeleteMessageReactionRequest.builder().message_id(…).reaction_id(…)` (adapter.py:2589-2592)
    /// from the handle in `_pending_processing_reactions` (adapter.py:2612-2618). So the cached
    /// handle is looked up first, and when there is none the call returns `false` having made **no
    /// request at all** — a guessed id is never deleted, since Feishu would either 404 or drop a
    /// badge this bot does not own.
    ///
    /// Blank `chat_id` / `message_id` / `emoji` are refused the same way, with zero requests
    /// (hermes: `if not message_id or not reaction_id: return False`, adapter.py:2586). `chat_id`
    /// is validated but not transmitted — the endpoint is message-scoped.
    ///
    /// A confirmed delete (`code == 0`, or a bare 2xx with no `code`) drops the cache entry, so the
    /// next remove for that pair again refuses locally. Any failure — `code != 0`, 403/404/429/5xx
    /// or a transport error — keeps the entry so a retry still knows the id, mirroring hermes'
    /// "keep the handle" branch (adapter.py:2614-2617).
    async fn remove_reaction(&self, chat_id: &str, message_id: &str, emoji: &str) -> bool {
        if chat_id.trim().is_empty() {
            return false;
        }
        let message_id = message_id.trim();
        if message_id.is_empty() {
            return false;
        }
        let Some(emoji_type) = reaction_emoji_type(emoji) else {
            return false;
        };
        let Some(reaction_id) = self.cached_reaction_id(message_id, &emoji_type) else {
            return false;
        };
        let request = HttpRequest::delete(self.reaction_url(message_id, &reaction_id));
        match self.reaction_request(request, "reaction delete").await {
            Ok(_) => {
                self.forget_reaction_id(message_id, &emoji_type);
                true
            }
            Err(_) => false,
        }
    }

    /// `send` chunks natively; the delivery router must not truncate first.
    fn splits_long_messages(&self) -> bool {
        true
    }

    /// The per-message cap `send` chunks against, so the router and the sender agree.
    fn max_message_length(&self) -> usize {
        MAX_MESSAGE_LENGTH
    }

    /// Feishu counts code points, which is already the trait default; stated explicitly so the cap
    /// and the chunker agree on the unit.
    fn message_length(&self, text: &str) -> usize {
        codepoint_len(text)
    }

    /// No-op: the Feishu bot API exposes no typing indicator (hermes `send_typing` is empty for the
    /// same reason). hermes badges the turn lifecycle with reactions instead — `Typing` while it
    /// works, `CrossMark` on failure (adapter.py:196-197 / 2596-2620) — and a host dispatch loop can
    /// do the same through [`add_reaction`](Self::add_reaction) with [`REACTION_IN_PROGRESS`] /
    /// [`REACTION_FAILURE`], then [`remove_reaction`](Self::remove_reaction); the adapter owns the
    /// per-message handle that delete needs, nothing else about the lifecycle.
    async fn send_typing(&self, _chat_id: &str, _metadata: Option<Value>) {}

    /// Honest refusal: Feishu's `PATCH /im/v1/messages/{id}` only updates interactive cards, so a
    /// bot's already-sent text message cannot be edited. Callers fall back to a fresh `send`; the
    /// streaming delivery path treats `bad_format` as "edit unsupported, send instead".
    async fn edit_message(
        &self,
        _chat_id: &str,
        _message_id: &str,
        _content: &str,
        _finalize: bool,
    ) -> SendResult {
        SendResult::failed(
            "feishu does not support editing sent messages; send a new one",
            Some("bad_format"),
        )
    }
}

/// How one Open Platform call failed, carrying everything the send taxonomy needs.
#[derive(Debug)]
struct Failure {
    message: String,
    kind: &'static str,
    retryable: bool,
    retry_after: Option<f64>,
    /// The body blamed the tenant token, so the cache entry is worthless.
    token_invalid: bool,
}

impl Failure {
    /// A request that never produced a usable response: safe to retry.
    fn transient(message: impl Into<String>) -> Self {
        Self {
            message: message.into(),
            kind: "transient",
            retryable: true,
            retry_after: None,
            token_invalid: false,
        }
    }

    /// A response we cannot classify beyond "it did not work".
    fn unknown(message: impl Into<String>) -> Self {
        Self {
            message: message.into(),
            kind: "unknown",
            retryable: false,
            retry_after: None,
            token_invalid: false,
        }
    }

    /// The shared [`SendResult`] shape: `error_kind` always set, `retryable`/`retry_after` only when
    /// the failure is worth another attempt.
    fn into_send_result(self) -> SendResult {
        let mut result = SendResult::failed(self.message, Some(self.kind));
        if self.retryable {
            result = result.retryable(self.retry_after);
        }
        result
    }
}

/// Feishu's failure taxonomy: the body's `code`/`msg` first (it is more specific than the status),
/// then the HTTP status.
///
/// - 99991663 / 99991661 (tenant token missing, expired, invalid) → `forbidden`, token invalidated;
/// - 230002 / 230001 (chat or user not found) → `not_found`, and so is a bare HTTP 404 (a chat or
///   resource the Open Platform could not route);
/// - 230020 or HTTP 429 (frequency limit) → `rate_limited`, retryable, honouring `Retry-After`;
/// - a bare HTTP 403 (no Feishu code) → `forbidden`: the app may not post in that chat;
/// - any other 4xx → `bad_format`;
/// - 5xx → `transient`, retryable;
/// - anything else (1xx/3xx, or a 2xx that still carried a non-zero `code`) → `transient`,
///   retryable: nothing was delivered, so a retry is the honest default.
///
/// The same classification runs for the media upload and for the message send, so both halves of a
/// media send report one taxonomy.
fn classify(status: u16, body: &Value, retry_after: Option<f64>) -> Failure {
    let code = body_code(body);
    let message = render_error(status, code, body);
    let token_invalid = code.is_some_and(|code| TOKEN_ERROR_CODES.contains(&code));
    let kind = if token_invalid {
        "forbidden"
    } else if code.is_some_and(|code| NOT_FOUND_ERROR_CODES.contains(&code)) {
        "not_found"
    } else if code == Some(FREQUENCY_LIMITED_CODE) || status == 429 {
        "rate_limited"
    } else {
        match status {
            403 => "forbidden",
            404 => "not_found",
            500..=599 => "transient",
            400..=499 => "bad_format",
            _ => "transient",
        }
    };
    let retryable = matches!(kind, "rate_limited" | "transient");
    Failure {
        message,
        kind,
        retryable,
        retry_after: if retryable { retry_after } else { None },
        token_invalid,
    }
}

/// A rejected credential pair at the token endpoint. There a 400/401/403 can only mean the app
/// credentials were refused, so it is reported as `forbidden` (non-retryable) rather than the
/// generic `bad_format` the send taxonomy would pick.
fn credentials_failure(status: u16, body: &Value) -> Failure {
    let mut failure = classify(status, body, None);
    if matches!(status, 400 | 401 | 403) {
        failure.kind = "forbidden";
        failure.retryable = false;
        failure.retry_after = None;
        failure.token_invalid = false;
    }
    failure
}

/// Feishu's `code` field, when the body carries one.
fn body_code(body: &Value) -> Option<i64> {
    body.get("code").and_then(Value::as_i64)
}

/// Human-readable error text: `[code] msg` when Feishu supplied a code, else the HTTP status.
fn render_error(status: u16, code: Option<i64>, body: &Value) -> String {
    let msg = body
        .get("msg")
        .and_then(Value::as_str)
        .map(str::trim)
        .filter(|msg| !msg.is_empty());
    match (code, msg) {
        (Some(code), Some(msg)) => format!("feishu error {code} (HTTP {status}): {msg}"),
        (Some(code), None) => format!("feishu error {code} (HTTP {status})"),
        (None, Some(msg)) => format!("feishu HTTP {status}: {msg}"),
        (None, None) => format!("feishu HTTP {status}"),
    }
}

/// The JSON type of `value`, so a refusal says what arrived instead of only what was wanted.
fn json_type_name(value: &Value) -> &'static str {
    match value {
        Value::Null => "null",
        Value::Bool(_) => "a boolean",
        Value::Number(_) => "a number",
        Value::String(_) => "a string",
        Value::Array(_) => "an array",
        Value::Object(_) => "an object",
    }
}

/// Is this chat the one-to-one conversation with the bot? `chat_mode` is the documented field;
/// `chat_type` is accepted as a fallback for payloads that only carry that one.
fn is_p2p_chat(data: Option<&Value>) -> bool {
    let Some(data) = data else {
        return false;
    };
    ["chat_mode", "chat_type"].iter().any(|key| {
        data.get(*key)
            .and_then(Value::as_str)
            .is_some_and(|value| value.trim().eq_ignore_ascii_case("p2p"))
    })
}

/// The gateway's platform handle for this adapter. The constant is a fixed non-empty name, so the
/// fallback arm is unreachable and exists only to keep this function panic-free.
fn feishu_platform() -> Platform {
    match Platform::new(platform_names::FEISHU) {
        Some(platform) => platform,
        None => Platform::local(),
    }
}

/// Percent-encode one URL path segment (unreserved characters kept, everything else `%XX`) so a
/// message or chat id can never re-shape the request path.
fn encode_path_segment(value: &str) -> String {
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

/// The `reaction_type.emoji_type` a caller's `emoji` becomes, or `None` when it is blank.
///
/// The reference does no translation: hermes' `_add_reaction` passes its `emoji_type` argument into
/// `CreateMessageReactionRequestBody…reaction_type({"emoji_type": emoji_type})` verbatim
/// (adapter.py:2572-2583) and every caller supplies a Feishu emoji *key* —
/// `_FEISHU_REACTION_IN_PROGRESS = "Typing"` / `_FEISHU_REACTION_FAILURE = "CrossMark"`
/// (adapter.py:196-197). There is no glyph→key table in the reference to mirror, so a Unicode emoji
/// travels as written in the same field: Feishu answers an unknown emoji with a non-zero `code`,
/// which the adapter reports as a failure instead of inventing a mapping. Only blankness is
/// rejected, matching hermes' `if not … emoji_type: return None` guard (adapter.py:2574).
fn reaction_emoji_type(emoji: &str) -> Option<String> {
    let trimmed = emoji.trim();
    if trimmed.is_empty() {
        return None;
    }
    Some(trimmed.to_string())
}

/// The `(message_id, emoji) → reaction_id` handles [`FeishuAdapter::remove_reaction`] deletes by.
///
/// Feishu's delete call takes the opaque id that create returned, never the emoji (hermes
/// `DeleteMessageReactionRequest.builder().message_id(…).reaction_id(…)`, adapter.py:2589-2592), so
/// the id has to be remembered between the two calls. hermes keys its map by `message_id` alone
/// (`self._pending_processing_reactions: OrderedDict[str, str]`, adapter.py:1347-1348) because its
/// lifecycle only ever puts one badge on a message; the trait is per-emoji, so the key is the pair.
///
/// The reference also bounds the map and drains it in insertion order once it overflows
/// (`_FEISHU_PROCESSING_REACTION_CACHE_SIZE`, adapter.py:198-201 / 2602-2606); [`store`](Self::store)
/// does the same with a FIFO queue kept beside the map.
#[derive(Debug, Default)]
struct ReactionCache {
    ids: HashMap<(String, String), String>,
    /// Insertion order of the keys in `ids`, so an overflowing cache can drop its oldest handle.
    order: VecDeque<(String, String)>,
}

impl ReactionCache {
    /// The handle recorded for `(message_id, emoji)`.
    fn get(&self, message_id: &str, emoji: &str) -> Option<String> {
        self.ids
            .get(&(message_id.to_string(), emoji.to_string()))
            .cloned()
    }

    /// Record `reaction_id` for `(message_id, emoji)`.
    ///
    /// Replacing an existing handle keeps that key's original queue slot: the reference's
    /// `cache[key] = id; cache.move_to_end(key)` (adapter.py:2602-2604) refreshes recency for the
    /// same reason, and either way the entry survives until the cache overflows.
    fn store(&mut self, message_id: &str, emoji: &str, reaction_id: &str) {
        let key = (message_id.to_string(), emoji.to_string());
        if self
            .ids
            .insert(key.clone(), reaction_id.to_string())
            .is_none()
        {
            self.order.push_back(key);
        }
        while self.ids.len() > REACTION_CACHE_CAPACITY {
            match self.order.pop_front() {
                Some(oldest) => {
                    self.ids.remove(&oldest);
                }
                None => break,
            }
        }
    }

    /// Drop the handle for `(message_id, emoji)`.
    fn remove(&mut self, message_id: &str, emoji: &str) {
        let key = (message_id.to_string(), emoji.to_string());
        self.ids.remove(&key);
        // Keep the queue in step, or a stale entry could evict a later re-insert of the same key.
        self.order.retain(|queued| queued != &key);
    }
}

/// A framed `multipart/form-data` body: the boundary token and the bytes to POST.
struct MultipartBody {
    boundary: String,
    body: Vec<u8>,
}

/// The single file part of an upload: the form field it fills, the filename its header advertises,
/// the content type it declares and the bytes themselves.
struct FilePart<'a> {
    field: &'a str,
    filename: &'a str,
    content_type: &'a str,
    bytes: &'a [u8],
}

/// Frame `fields` (short text parts) plus one file part as `multipart/form-data` (RFC 7578).
///
/// Every part opens with `--{boundary}\r\n`, carries its headers, one blank line, its value and a
/// trailing CRLF; the body closes with `--{boundary}--\r\n`. Only CRLF ends a line — a bare LF would
/// be read as part of the value. This is the shape `lark_oapi` puts on the wire for
/// `CreateImageRequest` / `CreateFileRequest` (hermes `_build_image_upload_body` /
/// `_build_file_upload_body`, adapter.py:4064-4083); the transport has no multipart helper, so the
/// framing lives here.
fn multipart_body(fields: &[(&str, String)], file: FilePart<'_>) -> MultipartBody {
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
    format!("vikramFeishuFormBoundary{sequence:016x}{nanos:032x}")
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

/// `data.{key}` of an upload response, when it is a non-blank string.
///
/// hermes reads it the same way (`_extract_response_field`, adapter.py:3807-3810) and treats an
/// absent key as a failed upload rather than an empty one.
fn key_field(body: &Value, key: &str) -> Option<String> {
    body.pointer(&format!("/data/{key}"))
        .and_then(Value::as_str)
        .map(str::trim)
        .filter(|value| !value.is_empty())
        .map(str::to_string)
}

/// The filename an upload may advertise: the last path component of `raw`, stripped of quotes and
/// control characters (a raw CRLF would end the multipart header line and split the part).
fn header_safe_filename(raw: &str) -> String {
    let basename = raw.rsplit(['/', '\\']).next().unwrap_or_default();
    let mut cleaned = String::with_capacity(basename.len());
    for ch in basename.chars() {
        if !ch.is_control() && ch != '"' {
            cleaned.push(ch);
        }
    }
    cleaned.trim().to_string()
}

/// The lowercase extension of `name` with its dot (`.pdf`), or `""` when it has none.
fn file_extension(name: &str) -> String {
    match name.rsplit_once('.') {
        Some((stem, extension)) if !stem.is_empty() && !extension.is_empty() => {
            format!(".{}", extension.to_ascii_lowercase())
        }
        _ => String::new(),
    }
}

/// The extension a MIME type implies, for callers that supply no filename — the same job
/// `mimetypes.guess_extension` does in hermes' `_derive_remote_filename` (adapter.py:2810-2816).
fn mime_extension(mime: &str) -> Option<&'static str> {
    let essence = mime.split(';').next().unwrap_or_default().trim();
    match essence.to_ascii_lowercase().as_str() {
        "image/png" => Some(".png"),
        "image/jpeg" | "image/jpg" => Some(".jpg"),
        "image/gif" => Some(".gif"),
        "image/webp" => Some(".webp"),
        "image/bmp" => Some(".bmp"),
        "video/mp4" => Some(".mp4"),
        "video/quicktime" => Some(".mov"),
        "audio/ogg" | "audio/opus" => Some(".ogg"),
        "audio/mpeg" => Some(".mp3"),
        "application/pdf" => Some(".pdf"),
        _ => None,
    }
}

/// The name a file is uploaded and displayed under.
///
/// hermes always has a real path and uses its basename (`send_image_file`, adapter.py:1929;
/// `_send_uploaded_file_message`, adapter.py:3697). This port may get no name at all, so a generic
/// stem per kind is used, and the extension is taken from the name, then the MIME type, then the
/// kind's safe default. The extension matters: it is what [`resolve_file_routing`] reads.
fn media_filename(media: &OutboundMedia) -> String {
    let stem = match media.filename.as_deref() {
        Some(raw) if !header_safe_filename(raw).is_empty() => header_safe_filename(raw),
        _ => match media.kind {
            MediaKind::Image => "image".to_string(),
            MediaKind::Video => "video".to_string(),
            MediaKind::Audio => "audio".to_string(),
            MediaKind::Document => "file".to_string(),
        },
    };
    if !file_extension(&stem).is_empty() {
        return stem;
    }
    let extension = media
        .mime
        .as_deref()
        .and_then(mime_extension)
        .or(match media.kind {
            MediaKind::Image => Some(".png"),
            MediaKind::Video => Some(".mp4"),
            // No transcoding here: an unknown audio format is not claimed to be Opus, so it takes
            // the "file attachment" branch hermes falls back to without ffmpeg (adapter.py:1871-1874).
            MediaKind::Audio | MediaKind::Document => None,
        });
    match extension {
        Some(extension) => format!("{stem}{extension}"),
        None => stem,
    }
}

/// The content type an upload part declares: the caller's MIME type, or
/// [`DEFAULT_UPLOAD_MIME`] when it gave none.
fn upload_content_type(media: &OutboundMedia) -> String {
    media
        .mime
        .as_deref()
        .map(str::trim)
        .filter(|mime| !mime.is_empty())
        .unwrap_or(DEFAULT_UPLOAD_MIME)
        .to_string()
}

/// Which message type a caller's [`MediaKind`] asks for, in hermes' vocabulary: `send_video` asks
/// for `"media"`, `send_voice` for `"audio"`, `send_document` for `"file"` (adapter.py:1897-1915).
fn requested_msg_type(kind: MediaKind) -> &'static str {
    match kind {
        MediaKind::Video => "media",
        MediaKind::Audio => "audio",
        MediaKind::Image | MediaKind::Document => "file",
    }
}

/// The upload `file_type` and message `msg_type` for `display_name`, port of hermes'
/// `_resolve_outbound_file_routing` (adapter.py:4091-4102).
///
/// - `.ogg`/`.opus` bytes only become an `audio` message (Lark's in-app voice recording) when the
///   caller asked for audio; an `.ogg` document stays a file;
/// - `.mp4`/`.mov`/`.avi`/`.m4v` become a `media` upload/message;
/// - `.pdf`/`.doc(x)`/`.xls(x)`/`.ppt(x)` keep their typed `file_type` but are a `file` message;
/// - anything else is a `stream` upload sent as a `file` attachment.
fn resolve_file_routing(display_name: &str, requested: &str) -> (&'static str, &'static str) {
    let extension = file_extension(display_name);
    if OPUS_UPLOAD_EXTENSIONS.contains(&extension.as_str()) && requested == "audio" {
        return ("opus", "audio");
    }
    if MEDIA_UPLOAD_EXTENSIONS.contains(&extension.as_str()) {
        return ("mp4", "media");
    }
    if let Some((_, file_type)) = DOC_UPLOAD_TYPES
        .iter()
        .find(|(suffix, _)| *suffix == extension)
    {
        return (file_type, "file");
    }
    (FILE_UPLOAD_TYPE, "file")
}

/// The local size gate, checked before any request.
///
/// Images are capped at [`MAX_IMAGE_BYTES`], everything else at [`MAX_FILE_BYTES`]; see
/// [`MAX_IMAGE_BYTES`] for why the gate is local rather than left to Feishu's non-zero `code`.
fn size_failure(kind: MediaKind, size: usize) -> Option<SendResult> {
    let limit = match kind {
        MediaKind::Image => MAX_IMAGE_BYTES,
        _ => MAX_FILE_BYTES,
    };
    if size <= limit {
        return None;
    }
    Some(SendResult::failed(
        format!("Media file exceeds Feishu limit ({size} > {limit} bytes)"),
        Some("too_long"),
    ))
}

/// Why URL-sourced media cannot be sent here, and what to do instead.
///
/// hermes never forwards a URL as an `image_key`: `send_image` downloads the http(s) URL through
/// `_download_remote_image` (SSRF-guarded, adapter.py:1953-1967 / 2779-2781) and uploads the bytes,
/// falling back to sending the URL as *text* only when the download itself fails. This port has no
/// fetcher, so a URL is refused with the reason and no request at all, instead of pretending it
/// went out.
fn url_refusal(url: &str) -> SendResult {
    SendResult::failed(
        format!(
            "feishu media from a URL is not supported: hermes downloads the URL itself and uploads \
             the bytes, and this port has no fetcher. Pass MediaSource::Bytes instead of \
             MediaSource::Url({url})."
        ),
        Some("bad_format"),
    )
}

/// Constant-time byte comparison, so a token check cannot be turned into a timing oracle.
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
/// A JSON value as an `i64`, accepting the numeric-string form Feishu uses for timestamps.
fn value_to_i64(value: &Value) -> Option<i64> {
    match value {
        Value::Number(number) => number.as_i64(),
        Value::String(text) => text.trim().parse::<i64>().ok(),
        _ => None,
    }
}

/// A trimmed, non-empty string field.
fn text_field(container: Option<&Value>, key: &str) -> Option<String> {
    container
        .and_then(|container| container.get(key))
        .and_then(Value::as_str)
        .map(str::trim)
        .filter(|value| !value.is_empty())
        .map(str::to_string)
}

/// `message.content` is a JSON *string* holding the body; a plain string is treated as the text
/// itself (hermes `_load_feishu_payload`).
fn parse_content(content: Option<&Value>) -> Value {
    match content {
        Some(Value::String(raw)) => {
            serde_json::from_str(raw).unwrap_or_else(|_| json!({ "text": raw }))
        }
        Some(other) => other.clone(),
        None => Value::Null,
    }
}

/// The text an attachment message carries (an image `alt`, a file `text`), or `""`.
fn attachment_text(content: &Value) -> String {
    text_field(Some(content), "text")
        .or_else(|| text_field(Some(content), "alt"))
        .unwrap_or_default()
}

/// Build the normalized [`MessageEvent`] from one `im.message.receive_v1` payload.
///
/// Returns `None` for a message this port cannot represent (no ids, or a type whose payload
/// walking is not ported) so [`FeishuAdapter::parse_webhook`] can skip it instead of faking one.
fn parse_message(root: &Value, event: &Value, message: &Value) -> Option<MessageEvent> {
    let chat_id = text_field(Some(message), "chat_id")?;
    let message_id = text_field(Some(message), "message_id")?;
    let raw_type = text_field(Some(message), "message_type")
        .unwrap_or_else(|| "text".to_string())
        .to_ascii_lowercase();
    let content = parse_content(message.get("content"));

    // `message_type` → gateway type. Feishu's native `audio` is an in-app voice recording, which
    // hermes classifies as VOICE so the gateway auto-transcribes it like every other voice note.
    let (message_type, text, media_key) = match raw_type.as_str() {
        "text" => (
            MessageType::Text,
            text_field(Some(&content), "text").unwrap_or_default(),
            None,
        ),
        "image" => (
            MessageType::Photo,
            attachment_text(&content),
            text_field(Some(&content), "image_key"),
        ),
        "file" => (
            MessageType::Document,
            attachment_text(&content),
            text_field(Some(&content), "file_key"),
        ),
        "audio" => (
            MessageType::Voice,
            attachment_text(&content),
            text_field(Some(&content), "file_key"),
        ),
        "media" | "video" => (
            MessageType::Video,
            attachment_text(&content),
            text_field(Some(&content), "file_key"),
        ),
        _ => return None,
    };

    let sender = event.get("sender");
    let sender_id = sender.and_then(|sender| sender.get("sender_id"));
    let open_id = text_field(sender_id, "open_id");
    let union_id = text_field(sender_id, "union_id");
    let primary_id = open_id.clone().or_else(|| text_field(sender_id, "user_id"));
    let sender_type = sender
        .and_then(|sender| sender.get("sender_type"))
        .and_then(Value::as_str)
        .unwrap_or_default();

    let mut source = SessionSource::new(feishu_platform(), chat_id);
    // Feishu chats are groups. hermes refines this with `GET /im/v1/chats/{id}` (`chat_mode:
    // "p2p"` ⇒ DM); webhook parsing is offline, so the group default stands here and a caller that
    // needs the exact class asks `get_chat_info`.
    source.chat_type = "group".to_string();
    source.user_id = primary_id.clone();
    source.user_id_alt = union_id;
    source.is_bot =
        sender_type.eq_ignore_ascii_case("bot") || sender_type.eq_ignore_ascii_case("app");
    source.message_id = Some(message_id.clone());

    let mut parsed = MessageEvent::new(text, source);
    parsed.message_type = message_type;
    // hermes mirrors the author onto the event for per-message prompt builders. Names need a
    // contact API call, so `user_name` stays empty rather than inventing one.
    parsed.user_id = primary_id;
    parsed.raw_message = Some(root.clone());
    parsed.message_id = Some(message_id);
    parsed.platform_update_id = message.get("create_time").and_then(value_to_i64);
    parsed.reply_to_message_id =
        text_field(Some(message), "parent_id").or_else(|| text_field(Some(message), "root_id"));
    if let Some(key) = media_key {
        // The key is what a follow-up `GET /im/v1/messages/{id}/resources/{file_key}` needs; the
        // event carries no MIME type and no name for every kind, so those stay empty.
        parsed.media_urls.push(key);
        if let Some(name) = text_field(Some(&content), "file_name") {
            parsed.media_filenames.push(name);
        }
    }
    Some(parsed)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::transport::{HttpMethod, HttpResponse, Transport, TransportError};
    use std::collections::VecDeque;
    use std::sync::{Arc, Mutex as StdMutex, MutexGuard};

    /// Records every request and replays scripted responses in order.
    struct MockTransport {
        requests: StdMutex<Vec<HttpRequest>>,
        responses: StdMutex<VecDeque<Result<HttpResponse, TransportError>>>,
    }

    impl MockTransport {
        fn new(responses: Vec<Result<HttpResponse, TransportError>>) -> Arc<Self> {
            Arc::new(Self {
                requests: StdMutex::new(Vec::new()),
                responses: StdMutex::new(responses.into()),
            })
        }

        fn requests(&self) -> Vec<HttpRequest> {
            lock(&self.requests).clone()
        }

        /// Requests that are not token fetches, i.e. the actual platform calls.
        fn api_requests(&self) -> Vec<HttpRequest> {
            self.requests()
                .into_iter()
                .filter(|request| !request.url.contains("/auth/v3/tenant_access_token"))
                .collect()
        }

        fn token_requests(&self) -> usize {
            self.requests().len() - self.api_requests().len()
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
    fn lock<T>(guard: &StdMutex<T>) -> MutexGuard<'_, T> {
        guard
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
    }

    fn request_body(request: &HttpRequest) -> Value {
        let bytes = request.body.clone().unwrap_or_default();
        serde_json::from_slice(&bytes).unwrap_or(Value::Null)
    }

    /// The chunk text carried by a create/reply request: `content` is a JSON *string*.
    fn request_text(request: &HttpRequest) -> String {
        let content = request_body(request)
            .get("content")
            .and_then(Value::as_str)
            .unwrap_or_default()
            .to_string();
        match serde_json::from_str::<Value>(&content) {
            Ok(parsed) => parsed
                .get("text")
                .and_then(Value::as_str)
                .unwrap_or_default()
                .to_string(),
            Err(_) => content,
        }
    }

    fn response(status: u16, body: Value) -> Result<HttpResponse, TransportError> {
        Ok(HttpResponse {
            status,
            body: body.to_string(),
            retry_after: None,
        })
    }

    fn response_with_retry_after(
        status: u16,
        body: Value,
        retry_after: f64,
    ) -> Result<HttpResponse, TransportError> {
        Ok(HttpResponse {
            status,
            body: body.to_string(),
            retry_after: Some(retry_after),
        })
    }

    fn token_ok(expire: i64) -> Result<HttpResponse, TransportError> {
        response(
            200,
            json!({
                "code": 0,
                "msg": "ok",
                "tenant_access_token": "t-tenant",
                "expire": expire,
            }),
        )
    }

    fn message_ok(message_id: &str) -> Result<HttpResponse, TransportError> {
        response(
            200,
            json!({ "code": 0, "msg": "success", "data": { "message_id": message_id } }),
        )
    }

    fn feishu_error(status: u16, code: i64, msg: &str) -> Result<HttpResponse, TransportError> {
        response(status, json!({ "code": code, "msg": msg }))
    }

    /// A successful `POST /im/v1/images` envelope.
    fn image_upload_ok(image_key: &str) -> Result<HttpResponse, TransportError> {
        response(
            200,
            json!({ "code": 0, "msg": "success", "data": { "image_key": image_key } }),
        )
    }

    /// A successful `POST /im/v1/files` envelope.
    fn file_upload_ok(file_key: &str) -> Result<HttpResponse, TransportError> {
        response(
            200,
            json!({ "code": 0, "msg": "success", "data": { "file_key": file_key } }),
        )
    }

    fn adapter(mock: &Arc<MockTransport>) -> FeishuAdapter {
        FeishuAdapter::new(mock.clone() as SharedTransport, "cli_app", "s3cret")
    }

    fn non_whitespace(text: &str) -> String {
        text.chars().filter(|ch| !ch.is_whitespace()).collect()
    }

    fn webhook_body(event_type: &str, event: Value) -> String {
        json!({
            "schema": "2.0",
            "header": {
                "event_id": "evt-1",
                "event_type": event_type,
                "create_time": "1700000000000",
                "token": "vt-token",
                "app_id": "cli_app",
            },
            "event": event,
        })
        .to_string()
    }

    #[tokio::test]
    async fn connect_fetches_a_tenant_token_and_later_sends_reuse_it() {
        let mock = MockTransport::new(vec![token_ok(7200), message_ok("om_1"), message_ok("om_2")]);
        let mut client = adapter(&mock);

        assert!(client.connect(false).await);
        assert!(client.send("oc_chat", "first", None, None).await.success);
        assert!(client.send("oc_chat", "second", None, None).await.success);

        // Two sends, one token fetch: the cached token is reused.
        assert_eq!(mock.token_requests(), 1);
        assert_eq!(mock.api_requests().len(), 2);

        let requests = mock.requests();
        assert_eq!(requests[0].method, HttpMethod::Post);
        assert_eq!(
            requests[0].url,
            "https://open.feishu.cn/open-apis/auth/v3/tenant_access_token/internal"
        );
        let body = request_body(&requests[0]);
        assert_eq!(body["app_id"], json!("cli_app"));
        assert_eq!(body["app_secret"], json!("s3cret"));

        // Every platform call carries the tenant token as a bearer.
        for request in mock.api_requests() {
            assert!(
                request
                    .headers
                    .iter()
                    .any(|(name, value)| name == "Authorization" && value == "Bearer t-tenant"),
                "missing bearer on {}",
                request.url
            );
        }
        assert_eq!(client.tenant_token().await.unwrap_or_default(), "t-tenant");
        assert_eq!(mock.token_requests(), 1);
    }

    #[tokio::test]
    async fn a_token_closer_than_the_safety_margin_is_not_cached() {
        let mock = MockTransport::new(vec![token_ok(30), token_ok(7200)]);
        let client = adapter(&mock);

        assert_eq!(client.tenant_token().await.unwrap_or_default(), "t-tenant");
        // `expire` is inside the 60 s margin, so nothing was cached and a second call re-fetches.
        assert_eq!(client.tenant_token().await.unwrap_or_default(), "t-tenant");
        assert_eq!(mock.token_requests(), 2);
    }

    #[tokio::test]
    async fn a_rejected_credential_pair_fails_connect_and_the_send() {
        let bad_secret = MockTransport::new(vec![
            feishu_error(400, 10014, "app secret invalid"),
            feishu_error(400, 10014, "app secret invalid"),
        ]);
        let mut rejected = adapter(&bad_secret);
        assert!(!rejected.connect(false).await);
        let result = rejected.send("oc_chat", "hello", None, None).await;
        assert!(!result.success);
        assert_eq!(result.error_kind.as_deref(), Some("forbidden"));
        assert!(!result.retryable);
        assert!(
            result
                .error
                .as_deref()
                .unwrap_or_default()
                .contains("app secret invalid"),
            "{:?}",
            result.error
        );
        // The refusal came from the token endpoint: no message was attempted.
        assert!(bad_secret.api_requests().is_empty());

        // A token endpoint that never answers is transient, not forbidden.
        let down = MockTransport::new(vec![
            Err(TransportError::Request("dns".to_string())),
            Err(TransportError::Request("dns".to_string())),
        ]);
        let mut unreachable = adapter(&down);
        assert!(!unreachable.connect(false).await);
        let result = unreachable.send("oc_chat", "hello", None, None).await;
        assert_eq!(result.error_kind.as_deref(), Some("transient"));
        assert!(result.retryable);

        // code != 0 inside a 200 envelope is a failure too.
        let envelope = MockTransport::new(vec![response(
            200,
            json!({ "code": 10003, "msg": "invalid param" }),
        )]);
        assert!(!adapter(&envelope).connect(false).await);
    }

    #[tokio::test]
    async fn send_posts_a_text_message_with_a_json_encoded_content_string() {
        let mock = MockTransport::new(vec![token_ok(7200), message_ok("om_42")]);
        let result = adapter(&mock)
            .send("oc_chat", "hello there", None, None)
            .await;

        assert!(result.success);
        assert_eq!(result.message_id.as_deref(), Some("om_42"));
        assert_eq!(result.continuation_message_ids, vec!["om_42"]);
        assert_eq!(result.error_kind, None);

        let requests = mock.api_requests();
        assert_eq!(requests.len(), 1);
        assert_eq!(requests[0].method, HttpMethod::Post);
        assert_eq!(
            requests[0].url,
            "https://open.feishu.cn/open-apis/im/v1/messages?receive_id_type=chat_id"
        );
        let body = request_body(&requests[0]);
        assert_eq!(body["receive_id"], json!("oc_chat"));
        assert_eq!(body["msg_type"], json!("text"));
        // `content` is a *string* holding `{"text": "..."}`, exactly as the Open Platform wants.
        let content = body["content"].as_str().expect("content is a string");
        let decoded: Value = serde_json::from_str(content).expect("content holds json");
        assert_eq!(decoded["text"], json!("hello there"));
        assert!(body.get("uuid").is_none());
    }

    #[tokio::test]
    async fn reply_to_posts_to_the_reply_endpoint() {
        let mock = MockTransport::new(vec![token_ok(7200), message_ok("om_reply")]);
        let result = adapter(&mock)
            .send("oc_chat", "answering", Some("om_parent"), None)
            .await;

        assert!(result.success);
        assert_eq!(result.message_id.as_deref(), Some("om_reply"));
        let requests = mock.api_requests();
        assert_eq!(requests.len(), 1);
        assert_eq!(
            requests[0].url,
            "https://open.feishu.cn/open-apis/im/v1/messages/om_parent/reply"
        );
        let body = request_body(&requests[0]);
        assert_eq!(body["msg_type"], json!("text"));
        assert!(
            body.get("receive_id").is_none(),
            "a reply targets the message, not the chat: {body}"
        );
        assert_eq!(request_text(&requests[0]), "answering");
    }

    #[tokio::test]
    async fn long_content_is_chunked_within_the_code_point_cap() {
        let content: String = (0..400)
            .map(|idx| format!("sentence {idx} with enough words to fill a chunk\n"))
            .collect();
        assert!(
            content.chars().count() > MAX_MESSAGE_LENGTH,
            "test content must exceed one message"
        );

        let expected_chunks = content.chars().count() / (MAX_MESSAGE_LENGTH / 2) + 2;
        let mut responses = vec![token_ok(7200)];
        responses.extend((0..expected_chunks).map(|idx| message_ok(&format!("om_{idx}"))));
        let mock = MockTransport::new(responses);
        let result = adapter(&mock)
            .send("oc_chat", &content, Some("om_parent"), None)
            .await;

        assert!(result.success, "{:?}", result.error);
        let requests = mock.api_requests();
        assert!(requests.len() > 1, "content should have been split");
        assert_eq!(mock.token_requests(), 1);

        for request in &requests {
            let text = request_text(request);
            assert!(
                text.chars().count() <= MAX_MESSAGE_LENGTH,
                "chunk of {} code points exceeds the cap: {text:?}",
                text.chars().count()
            );
            // Every chunk of a reply goes back to the anchor, keeping the split answer threaded.
            assert!(request.url.ends_with("/om_parent/reply"));
        }
        assert_eq!(result.continuation_message_ids.len(), requests.len());
        assert_eq!(
            result.message_id,
            result.continuation_message_ids.last().cloned()
        );
        // Chunking loses no visible content.
        let joined: String = requests.iter().map(request_text).collect();
        assert_eq!(non_whitespace(&joined), non_whitespace(&content));
    }

    #[tokio::test]
    async fn feishu_error_codes_map_onto_the_send_taxonomy() {
        // (status, code, msg, expected kind, retryable)
        let cases = [
            (401, 99_991_663, "Invalid access token", "forbidden", false),
            (
                403,
                99_991_661,
                "invalid tenant access token",
                "forbidden",
                false,
            ),
            (400, 230_002, "chat not found", "not_found", false),
            (400, 230_001, "user not found", "not_found", false),
            // A 200 envelope carrying an error code is still a failure.
            (200, 230_002, "chat not found", "not_found", false),
            (400, 230_020, "frequency limit", "rate_limited", true),
            (429, 99_991_000, "too many requests", "rate_limited", true),
            (400, 100_002, "invalid parameter", "bad_format", false),
            (502, 100_000, "bad gateway", "transient", true),
        ];
        for (status, code, msg, kind, retryable) in cases {
            let mock = MockTransport::new(vec![token_ok(7200), feishu_error(status, code, msg)]);
            let result = adapter(&mock).send("oc_chat", "hello", None, None).await;
            assert!(!result.success, "HTTP {status} code {code} must fail");
            assert_eq!(
                result.error_kind.as_deref(),
                Some(kind),
                "HTTP {status} code {code}"
            );
            assert_eq!(result.retryable, retryable, "HTTP {status} code {code}");
            assert!(
                result.error.as_deref().unwrap_or_default().contains(msg),
                "HTTP {status} code {code} lost the message: {:?}",
                result.error
            );
        }

        // 429 carries the server's Retry-After through.
        let limited = MockTransport::new(vec![
            token_ok(7200),
            response_with_retry_after(429, json!({ "code": 230020, "msg": "slow down" }), 12.0),
        ]);
        let result = adapter(&limited).send("oc_chat", "hello", None, None).await;
        assert_eq!(result.error_kind.as_deref(), Some("rate_limited"));
        assert!(result.retryable);
        assert_eq!(result.retry_after, Some(12.0));

        // A transport failure is transient and retryable.
        let down = MockTransport::new(vec![
            token_ok(7200),
            Err(TransportError::Request("connection reset".to_string())),
        ]);
        let result = adapter(&down).send("oc_chat", "hello", None, None).await;
        assert_eq!(result.error_kind.as_deref(), Some("transient"));
        assert!(result.retryable);
    }

    #[tokio::test]
    async fn a_token_error_code_drops_the_cached_token() {
        let mock = MockTransport::new(vec![
            token_ok(7200),
            feishu_error(401, 99_991_663, "Invalid access token"),
            token_ok(7200),
            message_ok("om_after"),
        ]);
        let client = adapter(&mock);

        let failed = client.send("oc_chat", "hello", None, None).await;
        assert_eq!(failed.error_kind.as_deref(), Some("forbidden"));
        // The stale token was thrown away, so the next send fetches a fresh one.
        let ok = client.send("oc_chat", "hello again", None, None).await;
        assert!(ok.success);
        assert_eq!(mock.token_requests(), 2);
    }

    #[tokio::test]
    async fn a_later_chunk_failure_reports_the_delivered_head() {
        let content = "y".repeat(MAX_MESSAGE_LENGTH * 2);
        let mock = MockTransport::new(vec![
            token_ok(7200),
            message_ok("om_head"),
            feishu_error(403, 99_991_663, "Invalid access token"),
        ]);
        let result = adapter(&mock).send("oc_chat", &content, None, None).await;

        assert!(!result.success);
        assert_eq!(result.error_kind.as_deref(), Some("forbidden"));
        // The chunk that landed is reported so a retry sends only the tail.
        assert_eq!(result.continuation_message_ids, vec!["om_head"]);
        assert!(result.message_id.is_none());
    }

    #[tokio::test]
    async fn blank_content_is_refused_without_a_request() {
        let mock = MockTransport::new(Vec::new());
        let result = adapter(&mock).send("oc_chat", "  \n\t ", None, None).await;
        assert!(!result.success);
        assert_eq!(result.error_kind.as_deref(), Some("bad_format"));
        assert!(mock.requests().is_empty(), "no request may be made");
    }

    #[tokio::test]
    async fn get_chat_info_maps_p2p_and_group_chats() {
        // A p2p chat with the bot is a DM; anything else is a group.
        let p2p = MockTransport::new(vec![
            token_ok(7200),
            response(
                200,
                json!({ "code": 0, "data": { "name": "Ann", "chat_mode": "p2p" } }),
            ),
        ]);
        let info = adapter(&p2p).get_chat_info("oc_p2p").await;
        assert_eq!(info.name, "Ann");
        assert_eq!(info.chat_type, "dm");
        assert_eq!(
            p2p.api_requests()[0].url,
            "https://open.feishu.cn/open-apis/im/v1/chats/oc_p2p"
        );
        assert_eq!(p2p.api_requests()[0].method, HttpMethod::Get);

        let group = MockTransport::new(vec![
            token_ok(7200),
            response(
                200,
                json!({ "code": 0, "data": { "name": "Team", "chat_mode": "group" } }),
            ),
        ]);
        let info = adapter(&group).get_chat_info("oc_group").await;
        assert_eq!(info.name, "Team");
        assert_eq!(info.chat_type, "group");

        // A mirror base is honoured and its trailing slash is trimmed.
        let mirror = MockTransport::new(vec![
            token_ok(7200),
            response(200, json!({ "code": 0, "data": { "name": "Mirrored" } })),
        ]);
        let info = adapter(&mirror)
            .with_api_base("https://mirror.test/open-apis/")
            .get_chat_info("oc_mirror")
            .await;
        assert_eq!(info.name, "Mirrored");
        assert_eq!(info.chat_type, "group");
        assert_eq!(
            mirror.api_requests()[0].url,
            "https://mirror.test/open-apis/im/v1/chats/oc_mirror"
        );

        // Any failure degrades to the chat id, still classified as a group.
        let failed = MockTransport::new(vec![feishu_error(400, 230_002, "chat not found")]);
        let info = adapter(&failed).get_chat_info("oc_missing").await;
        assert_eq!(info.name, "oc_missing");
        assert_eq!(info.chat_type, "group");
    }

    #[tokio::test]
    async fn edit_message_is_an_honest_refusal() {
        let mock = MockTransport::new(Vec::new());
        let result = adapter(&mock)
            .edit_message("oc_chat", "om_1", "new text", false)
            .await;
        assert!(!result.success);
        assert_eq!(result.error_kind.as_deref(), Some("bad_format"));
        assert_eq!(
            result.error.as_deref(),
            Some("feishu does not support editing sent messages; send a new one")
        );
        assert!(mock.requests().is_empty());
    }

    // ----- outbound media fixtures ------------------------------------------------------------

    /// The boundary token a multipart request declared in its `Content-Type` header.
    fn multipart_boundary_of(request: &HttpRequest) -> String {
        request
            .headers
            .iter()
            .find(|(name, _)| name.eq_ignore_ascii_case("Content-Type"))
            .and_then(|(_, value)| value.strip_prefix("multipart/form-data; boundary="))
            .map(str::to_string)
            .unwrap_or_default()
    }

    /// One parsed `multipart/form-data` part.
    #[derive(Debug, Default)]
    struct MultipartPart {
        name: String,
        filename: Option<String>,
        content_type: Option<String>,
        value: Vec<u8>,
    }

    /// Split `body` on every occurrence of `needle`, keeping the segments between the matches.
    fn split_on(body: &[u8], needle: &[u8]) -> Vec<Vec<u8>> {
        assert!(!needle.is_empty(), "an empty delimiter cannot split a body");
        let mut segments = Vec::new();
        let mut start = 0;
        let mut cursor = 0;
        while cursor + needle.len() <= body.len() {
            if &body[cursor..cursor + needle.len()] == needle {
                segments.push(body[start..cursor].to_vec());
                cursor += needle.len();
                start = cursor;
            } else {
                cursor += 1;
            }
        }
        segments.push(body[start..].to_vec());
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

    /// Parse a multipart body back into its parts, asserting CRLF-only framing on the way.
    ///
    /// The bare-LF scan covers the whole body, so the fixtures below are LF-free on purpose: a real
    /// upload may of course carry any byte in its file part.
    fn multipart_parts(request: &HttpRequest) -> Vec<MultipartPart> {
        let boundary = multipart_boundary_of(request);
        assert!(
            !boundary.is_empty(),
            "an upload must declare a boundary: {:?}",
            request.headers
        );
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
            segments.first().map(Vec::len),
            Some(0),
            "nothing may precede the first boundary"
        );
        assert_eq!(
            segments.last().map(Vec::as_slice),
            Some(b"--\r\n".as_slice()),
            "the body must close with --boundary--CRLF"
        );
        let mut parts = Vec::new();
        let last = segments.len().saturating_sub(2);
        for segment in segments.iter().skip(1).take(last) {
            let segment = trim_crlf(segment);
            let Some(split) = find_bytes(segment, b"\r\n\r\n") else {
                continue;
            };
            let head = String::from_utf8_lossy(&segment[..split]).to_string();
            let mut part = MultipartPart {
                value: segment[split + 4..].to_vec(),
                ..MultipartPart::default()
            };
            for line in head.split("\r\n") {
                if let Some(rest) = line.strip_prefix("Content-Disposition: form-data; ") {
                    for attribute in rest.split(';') {
                        let attribute = attribute.trim();
                        if let Some(name) = attribute.strip_prefix("name=") {
                            part.name = name.trim_matches('"').to_string();
                        } else if let Some(filename) = attribute.strip_prefix("filename=") {
                            part.filename = Some(filename.trim_matches('"').to_string());
                        }
                    }
                } else if let Some(content_type) = line.strip_prefix("Content-Type: ") {
                    part.content_type = Some(content_type.to_string());
                }
            }
            parts.push(part);
        }
        parts
    }

    /// The part named `name` in a parsed multipart body.
    fn part_named<'a>(parts: &'a [MultipartPart], name: &str) -> Option<&'a MultipartPart> {
        parts.iter().find(|part| part.name == name)
    }

    /// The `content` JSON a create/reply request carries, decoded from its JSON *string* form.
    fn sent_content(request: &HttpRequest) -> Value {
        let raw = request_body(request)
            .get("content")
            .and_then(Value::as_str)
            .unwrap_or_default()
            .to_string();
        serde_json::from_str(&raw).unwrap_or(Value::Null)
    }

    #[tokio::test]
    async fn supports_media_is_true() {
        let mock = MockTransport::new(Vec::new());
        assert!(adapter(&mock).supports_media());
        assert!(
            mock.requests().is_empty(),
            "the capability flag makes no request"
        );
    }

    #[tokio::test]
    async fn image_bytes_upload_then_message_send() {
        let mock = MockTransport::new(vec![
            token_ok(7200),
            image_upload_ok("img_v2_abc"),
            message_ok("om_image"),
        ]);
        let media = OutboundMedia::new(
            MediaKind::Image,
            MediaSource::Bytes(b"png-payload-no-newlines".to_vec()),
        )
        .with_filename("photo.png")
        .with_mime("image/png");
        let result = adapter(&mock).send_media("oc_chat", &media, None).await;

        assert!(result.success, "{:?}", result.error);
        assert_eq!(result.message_id.as_deref(), Some("om_image"));
        assert_eq!(result.error_kind, None);
        assert!(!result.retryable);
        assert!(result.continuation_message_ids.is_empty());

        // One token, one upload, one message.
        let requests = mock.api_requests();
        assert_eq!(requests.len(), 2, "one upload and one message");
        assert_eq!(mock.token_requests(), 1);

        let upload = &requests[0];
        assert_eq!(upload.method, HttpMethod::Post);
        assert_eq!(upload.url, "https://open.feishu.cn/open-apis/im/v1/images");
        assert!(
            upload
                .headers
                .iter()
                .any(|(name, value)| name == "Authorization" && value == "Bearer t-tenant"),
            "the upload must carry the tenant token: {:?}",
            upload.headers
        );
        let parts = multipart_parts(upload);
        assert_eq!(parts.len(), 2, "{parts:?}");
        assert_eq!(
            part_named(&parts, "image_type").map(|part| part.value.clone()),
            Some(b"message".to_vec()),
            "image_type must be `message`"
        );
        let image = part_named(&parts, "image");
        assert_eq!(
            image.map(|part| part.value.clone()),
            Some(b"png-payload-no-newlines".to_vec())
        );
        assert_eq!(
            image.and_then(|part| part.filename.clone()),
            Some("photo.png".to_string())
        );
        assert_eq!(
            image.and_then(|part| part.content_type.clone()),
            Some("image/png".to_string())
        );

        let message = &requests[1];
        assert_eq!(message.method, HttpMethod::Post);
        assert_eq!(
            message.url,
            "https://open.feishu.cn/open-apis/im/v1/messages?receive_id_type=chat_id"
        );
        let body = request_body(message);
        assert_eq!(body["receive_id"], json!("oc_chat"));
        assert_eq!(body["msg_type"], json!("image"));
        assert_eq!(sent_content(message), json!({ "image_key": "img_v2_abc" }));
        assert!(body.get("uuid").is_none());
    }

    #[tokio::test]
    async fn file_bytes_upload_with_stream_type_then_file_message() {
        let mock = MockTransport::new(vec![
            token_ok(7200),
            file_upload_ok("file_v2_doc"),
            message_ok("om_file"),
        ]);
        let media = OutboundMedia::new(
            MediaKind::Document,
            MediaSource::Bytes(b"notes-body-no-newlines".to_vec()),
        )
        .with_filename("notes.txt")
        .with_mime("text/plain");
        let result = adapter(&mock).send_media("oc_chat", &media, None).await;

        assert!(result.success, "{:?}", result.error);
        assert_eq!(result.message_id.as_deref(), Some("om_file"));

        let requests = mock.api_requests();
        assert_eq!(requests.len(), 2);
        let upload = &requests[0];
        assert_eq!(upload.method, HttpMethod::Post);
        assert_eq!(upload.url, "https://open.feishu.cn/open-apis/im/v1/files");
        assert!(
            upload
                .headers
                .iter()
                .any(|(name, value)| name == "Authorization" && value == "Bearer t-tenant")
        );

        let parts = multipart_parts(upload);
        assert_eq!(parts.len(), 3, "{parts:?}");
        assert_eq!(
            part_named(&parts, "file_type").map(|part| part.value.clone()),
            Some(b"stream".to_vec()),
            "an unclassified file uploads as `stream`"
        );
        assert_eq!(
            part_named(&parts, "file_name").map(|part| part.value.clone()),
            Some(b"notes.txt".to_vec())
        );
        let file = part_named(&parts, "file");
        assert_eq!(
            file.map(|part| part.value.clone()),
            Some(b"notes-body-no-newlines".to_vec())
        );
        assert_eq!(
            file.and_then(|part| part.filename.clone()),
            Some("notes.txt".to_string())
        );
        assert_eq!(
            file.and_then(|part| part.content_type.clone()),
            Some("text/plain".to_string())
        );

        let message = &requests[1];
        assert_eq!(
            message.url,
            "https://open.feishu.cn/open-apis/im/v1/messages?receive_id_type=chat_id"
        );
        assert_eq!(request_body(message)["msg_type"], json!("file"));
        assert_eq!(sent_content(message), json!({ "file_key": "file_v2_doc" }));
        assert!(request_body(message).get("uuid").is_none());
    }

    #[tokio::test]
    async fn video_audio_and_typed_documents_map_onto_feishu_msg_types() {
        // (kind, filename, mime, expected upload `file_type`, expected `msg_type`)
        let cases = [
            (MediaKind::Video, "clip.mp4", "video/mp4", "mp4", "media"),
            (
                MediaKind::Video,
                "clip.mov",
                "video/quicktime",
                "mp4",
                "media",
            ),
            (MediaKind::Audio, "voice.ogg", "audio/ogg", "opus", "audio"),
            (
                MediaKind::Document,
                "report.pdf",
                "application/pdf",
                "pdf",
                "file",
            ),
            (
                MediaKind::Document,
                "deck.pptx",
                "application/octet-stream",
                "ppt",
                "file",
            ),
            // An unknown audio format is not claimed as a voice note: the reference sends it as a
            // plain file attachment (send_voice without ffmpeg, adapter.py:1871-1874).
            (MediaKind::Audio, "song.mp3", "audio/mpeg", "stream", "file"),
            // `.ogg` only becomes an audio message when the caller asked for audio.
            (
                MediaKind::Document,
                "sample.ogg",
                "audio/ogg",
                "stream",
                "file",
            ),
        ];
        for (kind, filename, mime, file_type, msg_type) in cases {
            let mock = MockTransport::new(vec![
                token_ok(7200),
                file_upload_ok("file_v2_key"),
                message_ok("om_typed"),
            ]);
            let media =
                OutboundMedia::new(kind, MediaSource::Bytes(b"payload-no-newlines".to_vec()))
                    .with_filename(filename)
                    .with_mime(mime);
            let result = adapter(&mock).send_media("oc_chat", &media, None).await;

            assert!(result.success, "{filename}: {:?}", result.error);
            assert_eq!(result.message_id.as_deref(), Some("om_typed"), "{filename}");
            let requests = mock.api_requests();
            assert_eq!(requests.len(), 2, "{filename}");
            assert_eq!(
                requests[0].url, "https://open.feishu.cn/open-apis/im/v1/files",
                "{filename}"
            );
            let parts = multipart_parts(&requests[0]);
            assert_eq!(
                part_named(&parts, "file_type").map(|part| part.value.clone()),
                Some(file_type.as_bytes().to_vec()),
                "{filename} file_type"
            );
            assert_eq!(
                request_body(&requests[1])["msg_type"],
                json!(msg_type),
                "{filename} msg_type"
            );
            assert_eq!(
                sent_content(&requests[1]),
                json!({ "file_key": "file_v2_key" }),
                "{filename} content"
            );
        }
    }

    #[tokio::test]
    async fn media_reply_targets_the_reply_endpoint() {
        let mock = MockTransport::new(vec![
            token_ok(7200),
            image_upload_ok("img_v2_reply"),
            message_ok("om_reply"),
        ]);
        let media = OutboundMedia::new(MediaKind::Image, MediaSource::Bytes(b"png".to_vec()))
            .with_filename("a.png");
        let result = adapter(&mock)
            .send_media("oc_chat", &media, Some("om_parent"))
            .await;

        assert!(result.success, "{:?}", result.error);
        assert_eq!(result.message_id.as_deref(), Some("om_reply"));

        let requests = mock.api_requests();
        assert_eq!(requests.len(), 2);
        assert_eq!(
            requests[1].url,
            "https://open.feishu.cn/open-apis/im/v1/messages/om_parent/reply"
        );
        let body = request_body(&requests[1]);
        assert_eq!(body["msg_type"], json!("image"));
        assert!(
            body.get("receive_id").is_none(),
            "a reply targets the message, not the chat: {body}"
        );
        assert!(body.get("uuid").is_none());
        assert_eq!(
            sent_content(&requests[1]),
            json!({ "image_key": "img_v2_reply" })
        );
    }

    #[tokio::test]
    async fn an_upload_failure_stops_before_the_message_send() {
        let mock = MockTransport::new(vec![
            token_ok(7200),
            feishu_error(400, 234_001, "invalid image"),
        ]);
        let media = OutboundMedia::new(MediaKind::Image, MediaSource::Bytes(b"png".to_vec()))
            .with_filename("a.png");
        let result = adapter(&mock).send_media("oc_chat", &media, None).await;

        assert!(!result.success);
        assert_eq!(result.error_kind.as_deref(), Some("bad_format"));
        assert!(result.message_id.is_none());
        assert!(
            result
                .error
                .as_deref()
                .unwrap_or_default()
                .contains("invalid image"),
            "{:?}",
            result.error
        );
        let requests = mock.api_requests();
        assert_eq!(
            requests.len(),
            1,
            "no message may be sent after a failed upload"
        );
        assert!(
            requests[0].url.ends_with("/im/v1/images"),
            "{}",
            requests[0].url
        );
    }

    #[tokio::test]
    async fn an_upload_without_a_key_is_a_failure_not_an_empty_upload() {
        // HTTP 200 + `code: 0` but no `image_key`: hermes reports "image upload missing image_key".
        let no_key = MockTransport::new(vec![
            token_ok(7200),
            response(200, json!({ "code": 0, "msg": "success", "data": {} })),
        ]);
        let media = OutboundMedia::new(MediaKind::Image, MediaSource::Bytes(b"png".to_vec()))
            .with_filename("a.png");
        let result = adapter(&no_key).send_media("oc_chat", &media, None).await;
        assert!(!result.success);
        assert_eq!(result.error_kind.as_deref(), Some("unknown"));
        assert_eq!(
            result.error.as_deref(),
            Some("feishu image upload response carried no data.image_key")
        );
        assert!(result.message_id.is_none());
        assert_eq!(no_key.api_requests().len(), 1);

        // The file path says so about `file_key`.
        let no_file_key = MockTransport::new(vec![
            token_ok(7200),
            response(200, json!({ "code": 0, "msg": "success", "data": {} })),
        ]);
        let media = OutboundMedia::new(MediaKind::Document, MediaSource::Bytes(b"doc".to_vec()))
            .with_filename("a.txt");
        let result = adapter(&no_file_key)
            .send_media("oc_chat", &media, None)
            .await;
        assert!(!result.success);
        assert_eq!(result.error_kind.as_deref(), Some("unknown"));
        assert_eq!(
            result.error.as_deref(),
            Some("feishu file upload response carried no data.file_key")
        );
    }

    #[tokio::test]
    async fn a_message_failure_after_the_upload_is_surfaced() {
        let mock = MockTransport::new(vec![
            token_ok(7200),
            image_upload_ok("img_v2_uploaded"),
            feishu_error(403, 99_991_663, "Invalid access token"),
        ]);
        let media = OutboundMedia::new(MediaKind::Image, MediaSource::Bytes(b"png".to_vec()))
            .with_filename("a.png");
        let result = adapter(&mock).send_media("oc_chat", &media, None).await;

        assert!(
            !result.success,
            "an uploaded but unsent media is not a delivery"
        );
        assert_eq!(result.error_kind.as_deref(), Some("forbidden"));
        assert!(result.message_id.is_none());
        assert!(result.continuation_message_ids.is_empty());
        let requests = mock.api_requests();
        assert_eq!(
            requests.len(),
            2,
            "the upload happened, the message did not land"
        );
        assert!(
            requests[1]
                .url
                .contains("/im/v1/messages?receive_id_type=chat_id"),
            "{}",
            requests[1].url
        );
    }

    #[tokio::test]
    async fn media_send_failures_map_onto_the_send_taxonomy() {
        // (status, code, msg, expected kind, retryable)
        let cases = [
            (200, 230_002, "chat not found", "not_found", false),
            (403, 99_991_663, "Invalid access token", "forbidden", false),
            (403, 0, "no permission", "forbidden", false),
            (404, 0, "not found", "not_found", false),
            (400, 100_002, "invalid parameter", "bad_format", false),
            (429, 0, "too many requests", "rate_limited", true),
            (400, 230_020, "frequency limit", "rate_limited", true),
            (502, 100_000, "bad gateway", "transient", true),
        ];
        for (status, code, msg, kind, retryable) in cases {
            let mock = MockTransport::new(vec![
                token_ok(7200),
                image_upload_ok("img_v2_case"),
                feishu_error(status, code, msg),
            ]);
            let media = OutboundMedia::new(MediaKind::Image, MediaSource::Bytes(b"png".to_vec()))
                .with_filename("a.png");
            let result = adapter(&mock).send_media("oc_chat", &media, None).await;

            assert!(!result.success, "HTTP {status} code {code} must fail");
            assert_eq!(
                result.error_kind.as_deref(),
                Some(kind),
                "HTTP {status} code {code}"
            );
            assert_eq!(result.retryable, retryable, "HTTP {status} code {code}");
            assert!(
                result.error.as_deref().unwrap_or_default().contains(msg),
                "HTTP {status} code {code} lost the message: {:?}",
                result.error
            );
        }

        // 429 carries the server's Retry-After through the media path too.
        let limited = MockTransport::new(vec![
            token_ok(7200),
            image_upload_ok("img_v2_slow"),
            response_with_retry_after(429, json!({ "code": 230020, "msg": "slow down" }), 7.0),
        ]);
        let media = OutboundMedia::new(MediaKind::Image, MediaSource::Bytes(b"png".to_vec()));
        let result = adapter(&limited).send_media("oc_chat", &media, None).await;
        assert_eq!(result.error_kind.as_deref(), Some("rate_limited"));
        assert!(result.retryable);
        assert_eq!(result.retry_after, Some(7.0));

        // A transport failure on the message step is transient and retryable.
        let down = MockTransport::new(vec![
            token_ok(7200),
            image_upload_ok("img_v2_down"),
            Err(TransportError::Request("connection reset".to_string())),
        ]);
        let media = OutboundMedia::new(MediaKind::Image, MediaSource::Bytes(b"png".to_vec()));
        let result = adapter(&down).send_media("oc_chat", &media, None).await;
        assert_eq!(result.error_kind.as_deref(), Some("transient"));
        assert!(result.retryable);
    }

    #[tokio::test]
    async fn upload_failures_map_onto_the_send_taxonomy() {
        // (status, code, msg, expected kind, retryable)
        let cases = [
            (429, 0, "slow down", "rate_limited", true),
            (403, 0, "no permission", "forbidden", false),
            (404, 0, "no such endpoint", "not_found", false),
            (400, 100_002, "invalid parameter", "bad_format", false),
            (500, 0, "internal error", "transient", true),
            // A 2xx envelope that still carries a non-zero code is a failure, not an upload.
            (200, 234_001, "invalid file", "transient", true),
        ];
        for (status, code, msg, kind, retryable) in cases {
            let mock = MockTransport::new(vec![token_ok(7200), feishu_error(status, code, msg)]);
            let media =
                OutboundMedia::new(MediaKind::Document, MediaSource::Bytes(b"doc".to_vec()))
                    .with_filename("a.txt");
            let result = adapter(&mock).send_media("oc_chat", &media, None).await;

            assert!(!result.success, "HTTP {status} code {code} must fail");
            assert_eq!(
                result.error_kind.as_deref(),
                Some(kind),
                "HTTP {status} code {code}"
            );
            assert_eq!(result.retryable, retryable, "HTTP {status} code {code}");
            assert_eq!(
                mock.api_requests().len(),
                1,
                "HTTP {status} code {code}: the message step must not run"
            );
        }

        // A transport failure on the upload step is transient, with no message attempt.
        let down = MockTransport::new(vec![
            token_ok(7200),
            Err(TransportError::Request("connection reset".to_string())),
        ]);
        let media = OutboundMedia::new(MediaKind::Document, MediaSource::Bytes(b"doc".to_vec()))
            .with_filename("a.txt");
        let result = adapter(&down).send_media("oc_chat", &media, None).await;
        assert_eq!(result.error_kind.as_deref(), Some("transient"));
        assert!(result.retryable);
        assert_eq!(down.api_requests().len(), 1);
    }

    #[tokio::test]
    async fn a_media_upload_uses_a_fresh_boundary_per_request() {
        let mock = MockTransport::new(vec![
            token_ok(7200),
            image_upload_ok("img_v2_one"),
            message_ok("om_one"),
            file_upload_ok("file_v2_two"),
            message_ok("om_two"),
        ]);
        let client = adapter(&mock);
        let image = OutboundMedia::new(MediaKind::Image, MediaSource::Bytes(b"png".to_vec()));
        assert!(client.send_media("oc_chat", &image, None).await.success);
        let document = OutboundMedia::new(MediaKind::Document, MediaSource::Bytes(b"doc".to_vec()))
            .with_filename("a.txt");
        assert!(client.send_media("oc_chat", &document, None).await.success);

        let uploads = mock.api_requests();
        let first = multipart_boundary_of(&uploads[0]);
        let second = multipart_boundary_of(&uploads[2]);
        assert!(!first.is_empty() && !second.is_empty());
        assert_ne!(first, second, "each upload frames its own body");
    }

    #[tokio::test]
    async fn empty_media_is_refused_without_a_request() {
        let mock = MockTransport::new(Vec::new());
        for kind in [
            MediaKind::Image,
            MediaKind::Video,
            MediaKind::Audio,
            MediaKind::Document,
        ] {
            let media = OutboundMedia::new(kind, MediaSource::Bytes(Vec::new()));
            let result = adapter(&mock).send_media("oc_chat", &media, None).await;
            assert!(!result.success, "{kind:?}");
            assert_eq!(result.error_kind.as_deref(), Some("bad_format"), "{kind:?}");
            assert!(result.message_id.is_none(), "{kind:?}");
        }
        assert!(mock.requests().is_empty(), "no request may be made");
    }

    #[tokio::test]
    async fn url_media_is_refused_without_a_request() {
        let mock = MockTransport::new(Vec::new());
        let media = OutboundMedia::image_url("https://example.test/a.png");
        let result = adapter(&mock).send_media("oc_chat", &media, None).await;

        assert!(!result.success);
        assert_eq!(result.error_kind.as_deref(), Some("bad_format"));
        let error = result.error.as_deref().unwrap_or_default();
        assert!(error.contains("MediaSource::Bytes"), "{error}");
        assert!(error.contains("https://example.test/a.png"), "{error}");
        assert!(mock.requests().is_empty(), "no request may be made");
    }

    #[tokio::test]
    async fn oversized_media_is_refused_locally() {
        let mock = MockTransport::new(Vec::new());

        let image = OutboundMedia::new(
            MediaKind::Image,
            MediaSource::Bytes(vec![7u8; MAX_IMAGE_BYTES + 1]),
        );
        let result = adapter(&mock).send_media("oc_chat", &image, None).await;
        assert!(!result.success);
        assert_eq!(result.error_kind.as_deref(), Some("too_long"));

        let document = OutboundMedia::new(
            MediaKind::Document,
            MediaSource::Bytes(vec![7u8; MAX_FILE_BYTES + 1]),
        );
        let result = adapter(&mock).send_media("oc_chat", &document, None).await;
        assert!(!result.success);
        assert_eq!(result.error_kind.as_deref(), Some("too_long"));

        assert!(mock.requests().is_empty(), "no request may be made");
        // Exactly at each cap is not a local refusal.
        assert!(size_failure(MediaKind::Image, MAX_IMAGE_BYTES).is_none());
        assert!(size_failure(MediaKind::Video, MAX_FILE_BYTES).is_none());
        assert!(size_failure(MediaKind::Image, MAX_IMAGE_BYTES + 1).is_some());
    }

    #[tokio::test]
    async fn a_caption_is_not_carried_by_the_bare_key_message() {
        // hermes folds a caption into a `post` payload that embeds the media tag
        // (`_send_uploaded_key` → `_build_media_post_payload`, adapter.py:3748-3761 / 4085-4089),
        // which this port does not build: the media message stays the bare key message, and no
        // extra text message may be invented in its place.
        let mock = MockTransport::new(vec![
            token_ok(7200),
            image_upload_ok("img_v2_caption"),
            message_ok("om_caption"),
        ]);
        let media = OutboundMedia::new(MediaKind::Image, MediaSource::Bytes(b"png".to_vec()))
            .with_filename("a.png")
            .with_caption("look at this");
        let result = adapter(&mock).send_media("oc_chat", &media, None).await;

        assert!(result.success, "{:?}", result.error);
        let requests = mock.api_requests();
        assert_eq!(
            requests.len(),
            2,
            "no caption follow-up message may be invented"
        );
        assert_eq!(request_body(&requests[1])["msg_type"], json!("image"));
        assert_eq!(
            sent_content(&requests[1]),
            json!({ "image_key": "img_v2_caption" })
        );
        assert!(
            !request_body(&requests[1])
                .to_string()
                .contains("look at this"),
            "the caption must not be smuggled into the key message"
        );
    }

    #[test]
    fn file_routing_matches_the_reference_table() {
        assert_eq!(
            resolve_file_routing("voice.ogg", "audio"),
            ("opus", "audio")
        );
        assert_eq!(
            resolve_file_routing("voice.OPUS", "audio"),
            ("opus", "audio")
        );
        // Opus only wins when the caller asked for audio; a document send stays a file.
        assert_eq!(
            resolve_file_routing("voice.ogg", "file"),
            ("stream", "file")
        );
        assert_eq!(
            resolve_file_routing("voice.ogg", "media"),
            ("stream", "file")
        );
        assert_eq!(resolve_file_routing("clip.mp4", "media"), ("mp4", "media"));
        assert_eq!(resolve_file_routing("clip.mov", "file"), ("mp4", "media"));
        assert_eq!(resolve_file_routing("clip.avi", "file"), ("mp4", "media"));
        assert_eq!(resolve_file_routing("clip.m4v", "file"), ("mp4", "media"));
        // A container Feishu does not take as `media` falls back to a file attachment.
        assert_eq!(
            resolve_file_routing("clip.mkv", "media"),
            ("stream", "file")
        );
        assert_eq!(resolve_file_routing("report.pdf", "file"), ("pdf", "file"));
        assert_eq!(resolve_file_routing("report.doc", "file"), ("doc", "file"));
        assert_eq!(resolve_file_routing("report.docx", "file"), ("doc", "file"));
        assert_eq!(resolve_file_routing("sheet.xls", "file"), ("xls", "file"));
        assert_eq!(resolve_file_routing("sheet.xlsx", "file"), ("xls", "file"));
        assert_eq!(resolve_file_routing("deck.ppt", "file"), ("ppt", "file"));
        assert_eq!(resolve_file_routing("deck.pptx", "file"), ("ppt", "file"));
        assert_eq!(
            resolve_file_routing("notes.txt", "file"),
            ("stream", "file")
        );
        assert_eq!(
            resolve_file_routing("no-extension", "file"),
            ("stream", "file")
        );
        assert_eq!(resolve_file_routing(".hidden", "file"), ("stream", "file"));
    }

    #[test]
    fn media_filenames_and_content_types_follow_the_kind() {
        let bytes = || MediaSource::Bytes(vec![1u8, 2, 3]);
        let name_of = |media: &OutboundMedia| media_filename(media);

        // No filename and no MIME: a generic stem with the kind's safe extension.
        assert_eq!(
            name_of(&OutboundMedia::new(MediaKind::Image, bytes())),
            "image.png"
        );
        assert_eq!(
            name_of(&OutboundMedia::new(MediaKind::Video, bytes())),
            "video.mp4"
        );
        // Audio is not claimed to be Opus: it keeps an extension-less stem, so it lands as a file.
        assert_eq!(
            name_of(&OutboundMedia::new(MediaKind::Audio, bytes())),
            "audio"
        );
        assert_eq!(
            name_of(&OutboundMedia::new(MediaKind::Document, bytes())),
            "file"
        );

        // A name without an extension takes the MIME type's (`_derive_remote_filename`).
        assert_eq!(
            name_of(
                &OutboundMedia::new(MediaKind::Document, bytes())
                    .with_filename("report")
                    .with_mime("application/pdf")
            ),
            "report.pdf"
        );
        assert_eq!(
            name_of(
                &OutboundMedia::new(MediaKind::Audio, bytes())
                    .with_filename("song")
                    .with_mime("audio/mpeg; codecs=mp3")
            ),
            "song.mp3"
        );
        assert_eq!(
            name_of(
                &OutboundMedia::new(MediaKind::Audio, bytes())
                    .with_filename("note")
                    .with_mime("audio/ogg")
            ),
            "note.ogg"
        );

        // Paths are reduced to the basename, and header-unsafe characters are dropped.
        assert_eq!(
            name_of(
                &OutboundMedia::new(MediaKind::Document, bytes()).with_filename("../../etc/passwd")
            ),
            "passwd"
        );
        assert_eq!(
            name_of(
                &OutboundMedia::new(MediaKind::Image, bytes())
                    .with_filename("ev\"il\r\nX-Injected: yes.png")
            ),
            "evilX-Injected: yes.png"
        );

        // The declared part content type: the caller's MIME, else octet-stream.
        assert_eq!(
            upload_content_type(&OutboundMedia::new(MediaKind::Image, bytes())),
            "application/octet-stream"
        );
        assert_eq!(
            upload_content_type(&OutboundMedia::new(MediaKind::Image, bytes()).with_mime("   ")),
            "application/octet-stream"
        );
        assert_eq!(
            upload_content_type(
                &OutboundMedia::new(MediaKind::Video, bytes()).with_mime("video/mp4")
            ),
            "video/mp4"
        );

        assert_eq!(mime_extension("image/png; charset=binary"), Some(".png"));
        assert_eq!(mime_extension("audio/opus"), Some(".ogg"));
        assert_eq!(mime_extension("application/zip"), None);
        assert_eq!(file_extension("a.PDF"), ".pdf");
        assert_eq!(file_extension("archive.tar.gz"), ".gz");
        assert_eq!(file_extension("no ext"), "");
    }

    #[test]
    fn multipart_boundaries_avoid_the_payload() {
        let payload = b"vikramFeishuFormBoundary0000000000000000in-the-payload".to_vec();
        let body = multipart_body(
            &[("image_type", IMAGE_UPLOAD_TYPE.to_string())],
            FilePart {
                field: "image",
                filename: "a.png",
                content_type: "image/png",
                bytes: &payload,
            },
        );
        // The first candidate occurs in the payload, so it must not have been chosen.
        assert!(!contains_bytes(&payload, body.boundary.as_bytes()));
        assert!(
            body.body
                .starts_with(format!("--{}\r\n", body.boundary).as_bytes())
        );
        assert!(body.body.ends_with(b"--\r\n"));
    }

    #[tokio::test]
    async fn send_typing_makes_no_request() {
        let mock = MockTransport::new(Vec::new());
        adapter(&mock).send_typing("oc_chat", None).await;
        assert!(mock.requests().is_empty());
    }

    #[test]
    fn webhook_verification_requires_the_configured_token() {
        let body = json!({ "header": { "token": "vt-token" } });
        let legacy = json!({ "token": "vt-token" });

        // Unconfigured ⇒ fail closed, even for a payload that carries a token.
        let unconfigured = FeishuAdapter::new(MockTransport::new(Vec::new()), "a", "b");
        assert!(!unconfigured.verify_webhook(&body));
        assert!(!unconfigured.verify_webhook(&legacy));

        // An empty configured token is unconfigured, not a match-anything wildcard.
        let blank = FeishuAdapter::new(MockTransport::new(Vec::new()), "a", "b")
            .with_verification_token("   ");
        assert!(!blank.verify_webhook(&body));

        let configured = FeishuAdapter::new(MockTransport::new(Vec::new()), "a", "b")
            .with_verification_token("vt-token");
        assert!(configured.verify_webhook(&body));
        assert!(configured.verify_webhook(&legacy));
        assert!(!configured.verify_webhook(&json!({ "header": { "token": "wrong" } })));
        assert!(!configured.verify_webhook(&json!({ "token": "wrong" })));
        // A payload with no token at all is rejected while a token is configured.
        assert!(!configured.verify_webhook(&json!({ "header": { "event_type": "x" } })));
    }

    #[test]
    fn webhook_parses_a_text_message_event() {
        let client = FeishuAdapter::new(MockTransport::new(Vec::new()), "a", "b");
        let body = webhook_body(
            MESSAGE_RECEIVE_EVENT,
            json!({
                "sender": {
                    "sender_id": { "open_id": "ou_sender", "union_id": "on_sender" },
                    "sender_type": "user",
                },
                "message": {
                    "message_id": "om_text",
                    "chat_id": "oc_chat",
                    "chat_type": "group",
                    "message_type": "text",
                    "create_time": "1700000000123",
                    "content": "{\"text\":\"hello there\"}",
                    "parent_id": "om_parent",
                },
            }),
        );
        let events = client.parse_webhook(&body).unwrap_or_default();

        assert_eq!(events.len(), 1);
        let event = &events[0];
        assert_eq!(event.text, "hello there");
        assert_eq!(event.message_type, MessageType::Text);
        assert_eq!(event.message_id.as_deref(), Some("om_text"));
        assert_eq!(event.platform_update_id, Some(1_700_000_000_123));
        assert_eq!(event.reply_to_message_id.as_deref(), Some("om_parent"));
        assert_eq!(event.source.platform.as_str(), "feishu");
        assert_eq!(event.source.chat_id, "oc_chat");
        assert_eq!(event.source.chat_type, "group");
        assert_eq!(event.source.user_id.as_deref(), Some("ou_sender"));
        assert_eq!(event.source.user_id_alt.as_deref(), Some("on_sender"));
        assert_eq!(event.source.message_id.as_deref(), Some("om_text"));
        assert!(!event.source.is_bot);
        assert_eq!(event.user_id.as_deref(), Some("ou_sender"));
        assert!(event.user_name.is_none(), "names need a contact API call");
        assert!(event.raw_message.is_some());
        assert!(event.media_urls.is_empty());
        assert!(event.reply_to_text.is_none(), "no message lookup is ported");
    }

    #[test]
    fn webhook_parses_media_messages_with_their_resource_keys() {
        let client = FeishuAdapter::new(MockTransport::new(Vec::new()), "a", "b");
        let cases = [
            (
                "image",
                json!({ "image_key": "img_v2_abc" }),
                MessageType::Photo,
                "img_v2_abc",
            ),
            (
                "file",
                json!({ "file_key": "file_v2_doc", "file_name": "notes.pdf" }),
                MessageType::Document,
                "file_v2_doc",
            ),
            (
                "audio",
                json!({ "file_key": "file_v2_voice", "duration": 3000 }),
                MessageType::Voice,
                "file_v2_voice",
            ),
            (
                "media",
                json!({ "file_key": "file_v2_video", "file_name": "clip.mp4" }),
                MessageType::Video,
                "file_v2_video",
            ),
        ];
        for (raw_type, content, expected_type, expected_key) in cases {
            let body = webhook_body(
                MESSAGE_RECEIVE_EVENT,
                json!({
                    "sender": { "sender_id": { "open_id": "ou_1" }, "sender_type": "user" },
                    "message": {
                        "message_id": format!("om_{raw_type}"),
                        "chat_id": "oc_chat",
                        "message_type": raw_type,
                        // `content` is always the JSON *string* the Open Platform sends.
                        "content": content.to_string(),
                    },
                }),
            );
            let events = client.parse_webhook(&body).unwrap_or_default();
            assert_eq!(events.len(), 1, "{raw_type}");
            let event = &events[0];
            assert_eq!(event.message_type, expected_type, "{raw_type}");
            assert_eq!(
                event.media_urls,
                vec![expected_key.to_string()],
                "{raw_type}"
            );
            assert_eq!(event.text, "", "{raw_type} carries no caption here");
        }

        // A file name, when the payload has one, rides on `media_filenames`.
        let body = webhook_body(
            MESSAGE_RECEIVE_EVENT,
            json!({
                "message": {
                    "message_id": "om_file",
                    "chat_id": "oc_chat",
                    "message_type": "file",
                    "content": json!({ "file_key": "file_v2_x", "file_name": "a.pdf" }).to_string(),
                },
            }),
        );
        let events = client.parse_webhook(&body).unwrap_or_default();
        assert_eq!(events.len(), 1);
        assert_eq!(events[0].media_filenames, vec!["a.pdf".to_string()]);
    }

    #[test]
    fn webhook_skips_unrelated_events_and_unmapped_message_types() {
        let client = FeishuAdapter::new(MockTransport::new(Vec::new()), "a", "b");

        // Card actions, member churn and the url_verification handshake carry no message.
        for other in [
            "im.chat.member.bot.added_v1",
            "card.action.trigger",
            "im.message.reaction.created_v1",
            "im.message.recalled_v1",
        ] {
            let body = webhook_body(
                other,
                json!({ "message": { "chat_id": "oc", "message_id": "om" } }),
            );
            assert!(
                client.parse_webhook(&body).unwrap_or_default().is_empty(),
                "{other} must not produce an event"
            );
        }

        // A message type this port cannot walk, and a message missing its ids, are skipped.
        for message in [
            json!({
                "message_id": "om_post",
                "chat_id": "oc",
                "message_type": "post",
                "content": "{\"zh_cn\":{\"content\":[[{\"tag\":\"text\",\"text\":\"hi\"}]]}}",
            }),
            json!({ "message_id": "om_no_chat", "message_type": "text", "content": "{\"text\":\"hi\"}" }),
            json!({ "chat_id": "oc", "message_type": "text", "content": "{\"text\":\"hi\"}" }),
        ] {
            let body = webhook_body(MESSAGE_RECEIVE_EVENT, json!({ "message": message }));
            assert!(client.parse_webhook(&body).unwrap_or_default().is_empty());
        }

        // Invalid JSON is an error, not a silent skip.
        let error = client
            .parse_webhook("not json")
            .expect_err("invalid json must fail");
        assert!(error.contains("invalid json"), "{error}");
    }

    #[test]
    fn helpers_encode_paths_and_classify_unknown_bodies() {
        assert_eq!(encode_path_segment("om_abc-1"), "om_abc-1");
        assert_eq!(encode_path_segment("a/b"), "a%2Fb");
        assert_eq!(body_code(&json!({ "code": 0 })), Some(0));
        assert_eq!(body_code(&json!({ "msg": "no code" })), None);
        assert_eq!(
            value_to_i64(&json!("1700000000123")),
            Some(1_700_000_000_123)
        );
        assert_eq!(value_to_i64(&json!(42)), Some(42));
        assert_eq!(value_to_i64(&json!("abc")), None);
        // Error text leads with Feishu's code when there is one, and the status when there is not.
        assert_eq!(
            render_error(200, Some(230_002), &json!({ "msg": "chat not found" })),
            "feishu error 230002 (HTTP 200): chat not found"
        );
        assert_eq!(render_error(500, None, &json!({})), "feishu HTTP 500");
        assert!(constant_time_eq(b"token", b"token"));
        assert!(!constant_time_eq(b"token", b"tokeN"));
        assert!(!constant_time_eq(b"token", b"tok"));
    }

    // ----- interactive cards ------------------------------------------------------------------

    /// The card hermes' `_card` builds (adapter.py:1265-1274): config + header + elements.
    fn sample_card() -> Value {
        json!({
            "config": { "wide_screen_mode": true },
            "header": {
                "title": { "tag": "plain_text", "content": "Deploy finished" },
                "template": "green",
            },
            "elements": [
                { "tag": "markdown", "content": "All tests green" },
                { "tag": "action", "actions": [
                    { "tag": "button", "text": { "tag": "plain_text", "content": "Open" },
                      "type": "primary", "value": { "hermes_action": "open" } }
                ] },
            ],
        })
    }

    #[tokio::test]
    async fn supports_cards_is_true_without_a_request() {
        let mock = MockTransport::new(Vec::new());
        assert!(adapter(&mock).supports_cards());
        assert!(
            mock.requests().is_empty(),
            "the capability flag makes no request"
        );
    }

    #[tokio::test]
    async fn a_card_is_sent_as_an_interactive_message_with_the_wrapped_content() {
        let mock = MockTransport::new(vec![token_ok(7200), message_ok("om_card")]);
        let card = sample_card();
        let result = adapter(&mock).send_card("oc_chat", &card).await;

        assert!(result.success, "{:?}", result.error);
        assert_eq!(result.message_id.as_deref(), Some("om_card"));
        assert!(result.error.is_none());
        assert_eq!(result.error_kind, None);
        assert!(!result.retryable);
        assert!(result.continuation_message_ids.is_empty());

        // One token fetch, one message: the card takes the ordinary message path.
        assert_eq!(mock.token_requests(), 1);
        let requests = mock.api_requests();
        assert_eq!(requests.len(), 1);
        assert_eq!(requests[0].method, HttpMethod::Post);
        assert_eq!(
            requests[0].url,
            "https://open.feishu.cn/open-apis/im/v1/messages?receive_id_type=chat_id"
        );
        assert!(
            requests[0]
                .headers
                .iter()
                .any(|(name, value)| name == "Authorization" && value == "Bearer t-tenant"),
            "the card send must carry the tenant token: {:?}",
            requests[0].headers
        );

        let body = request_body(&requests[0]);
        assert_eq!(body["receive_id"], json!("oc_chat"));
        assert_eq!(body["msg_type"], json!("interactive"));
        // `content` is a JSON *string*, and the card travels under its documented wrapper.
        let content = body["content"].as_str().unwrap_or_default().to_string();
        let decoded: Value = serde_json::from_str(&content).unwrap_or(Value::Null);
        assert_eq!(decoded, json!({ "card": sample_card() }));
        // A card is created in the chat: no reply target, and no `uuid` idempotency key.
        assert!(body.get("uuid").is_none(), "{body}");
        assert_eq!(sent_content(&requests[0]), json!({ "card": sample_card() }));
    }

    #[tokio::test]
    async fn invalid_cards_are_refused_with_zero_requests() {
        let mock = MockTransport::new(Vec::new());
        let client = adapter(&mock);
        let cases: Vec<(&str, Value)> = vec![
            ("a string", json!("{\"header\": {}}")),
            ("a number", json!(7)),
            ("null", Value::Null),
            ("an array", json!([{ "tag": "markdown", "content": "hi" }])),
            ("a boolean", json!(true)),
        ];
        for (label, card) in cases {
            let result = client.send_card("oc_chat", &card).await;
            assert!(!result.success, "{label} must not report success");
            assert_eq!(
                result.error_kind.as_deref(),
                Some("bad_format"),
                "{label}: {:?}",
                result.error
            );
            assert!(result.message_id.is_none(), "{label}");
            assert!(!result.retryable, "{label}");
            assert!(
                result
                    .error
                    .as_deref()
                    .unwrap_or_default()
                    .starts_with("feishu send card:"),
                "{label}: {:?}",
                result.error
            );
        }
        assert!(
            mock.requests().is_empty(),
            "an invalid card must never reach the wire, token included"
        );

        // The refusal names what arrived.
        let refusal = client.send_card("oc_chat", &json!("nope")).await;
        assert!(
            refusal
                .error
                .as_deref()
                .unwrap_or_default()
                .contains("got a string"),
            "{:?}",
            refusal.error
        );

        // A blank chat id is refused before the card is even inspected.
        let blank = client.send_card("   ", &sample_card()).await;
        assert!(!blank.success);
        assert_eq!(blank.error_kind.as_deref(), Some("bad_format"));
        assert!(mock.requests().is_empty());
    }

    #[tokio::test]
    async fn a_non_zero_card_code_is_a_typed_failure() {
        // HTTP 200 with a non-zero `code` delivered nothing: the envelope decides.
        let refused = MockTransport::new(vec![
            token_ok(7200),
            feishu_error(200, 230_025, "card content invalid"),
        ]);
        let result = adapter(&refused).send_card("oc_chat", &sample_card()).await;
        assert!(!result.success);
        assert_eq!(result.error_kind.as_deref(), Some("transient"));
        assert!(result.retryable);
        assert!(result.message_id.is_none());
        assert!(
            result
                .error
                .as_deref()
                .unwrap_or_default()
                .contains("card content invalid"),
            "{:?}",
            result.error
        );

        // The frequency-limit code is a rate limit, not an unknown fault.
        let limited = MockTransport::new(vec![
            token_ok(7200),
            feishu_error(200, 230_020, "frequency limit"),
        ]);
        let result = adapter(&limited).send_card("oc_chat", &sample_card()).await;
        assert_eq!(result.error_kind.as_deref(), Some("rate_limited"));
        assert!(result.retryable);

        // A 2xx with `code: 0` but no `message_id` is not a delivery either.
        let empty = MockTransport::new(vec![
            token_ok(7200),
            response(200, json!({ "code": 0, "msg": "success", "data": {} })),
        ]);
        let result = adapter(&empty).send_card("oc_chat", &sample_card()).await;
        assert!(!result.success);
        assert_eq!(result.error_kind.as_deref(), Some("unknown"));
        assert_eq!(
            result.error.as_deref(),
            Some("feishu send response carried no data.message_id")
        );
    }

    #[tokio::test]
    async fn card_status_failures_map_onto_the_send_taxonomy() {
        // (status, code, msg, expected kind, retryable)
        let cases = [
            (403, 0, "no permission", "forbidden", false),
            (403, 99_991_663, "Invalid access token", "forbidden", false),
            (404, 0, "not found", "not_found", false),
            (400, 230_002, "chat not found", "not_found", false),
            (400, 100_002, "invalid parameter", "bad_format", false),
            (429, 0, "too many requests", "rate_limited", true),
            (502, 100_000, "bad gateway", "transient", true),
        ];
        for (status, code, msg, kind, retryable) in cases {
            let mock = MockTransport::new(vec![token_ok(7200), feishu_error(status, code, msg)]);
            let result = adapter(&mock).send_card("oc_chat", &sample_card()).await;

            assert!(!result.success, "HTTP {status} code {code} must fail");
            assert_eq!(
                result.error_kind.as_deref(),
                Some(kind),
                "HTTP {status} code {code}"
            );
            assert_eq!(result.retryable, retryable, "HTTP {status} code {code}");
            assert!(result.message_id.is_none(), "HTTP {status} code {code}");
            assert!(
                result.error.as_deref().unwrap_or_default().contains(msg),
                "HTTP {status} code {code} lost the message: {:?}",
                result.error
            );
            assert_eq!(mock.api_requests().len(), 1, "HTTP {status} code {code}");
        }

        // 429 carries the server's Retry-After through.
        let limited = MockTransport::new(vec![
            token_ok(7200),
            response_with_retry_after(429, json!({ "code": 230020, "msg": "slow down" }), 6.0),
        ]);
        let result = adapter(&limited).send_card("oc_chat", &sample_card()).await;
        assert_eq!(result.error_kind.as_deref(), Some("rate_limited"));
        assert!(result.retryable);
        assert_eq!(result.retry_after, Some(6.0));

        // A transport failure is transient and retryable.
        let down = MockTransport::new(vec![
            token_ok(7200),
            Err(TransportError::Request("connection reset".to_string())),
        ]);
        let result = adapter(&down).send_card("oc_chat", &sample_card()).await;
        assert!(
            !result.success,
            "a request that never landed is no delivery"
        );
        assert_eq!(result.error_kind.as_deref(), Some("transient"));
        assert!(result.retryable);
        assert!(result.message_id.is_none());
        assert!(
            result
                .error
                .as_deref()
                .unwrap_or_default()
                .contains("connection reset"),
            "{:?}",
            result.error
        );
    }

    // ----- reactions --------------------------------------------------------------------------

    /// A successful `POST /im/v1/messages/{message_id}/reactions` envelope.
    fn reaction_created(reaction_id: &str) -> Result<HttpResponse, TransportError> {
        response(
            200,
            json!({
                "code": 0,
                "msg": "success",
                "data": { "reaction_id": reaction_id, "user_id": "cli_app" },
            }),
        )
    }

    /// A successful `DELETE /im/v1/messages/{message_id}/reactions/{reaction_id}` envelope.
    fn reaction_deleted() -> Result<HttpResponse, TransportError> {
        response(200, json!({ "code": 0, "msg": "success", "data": {} }))
    }

    /// The create request hermes builds (`_add_reaction`, adapter.py:2572-2583), for the paths that
    /// need to observe the classification behind the `bool` surface.
    fn add_reaction_request(
        client: &FeishuAdapter,
        message_id: &str,
        emoji_type: &str,
    ) -> HttpRequest {
        HttpRequest::json(
            client.reactions_url(message_id),
            &json!({ "reaction_type": { "emoji_type": emoji_type } }),
        )
    }

    #[tokio::test]
    async fn supports_reactions_is_true_without_a_request() {
        let mock = MockTransport::new(Vec::new());
        assert!(adapter(&mock).supports_reactions());
        assert!(
            mock.requests().is_empty(),
            "the capability flag makes no request"
        );
    }

    #[tokio::test]
    async fn add_reaction_posts_the_reaction_type_and_caches_the_returned_id() {
        let mock = MockTransport::new(vec![token_ok(7200), reaction_created("re_1")]);
        let client = adapter(&mock);

        assert!(
            client
                .add_reaction("oc_chat", "om_1", REACTION_IN_PROGRESS)
                .await
        );

        assert_eq!(mock.token_requests(), 1);
        let requests = mock.api_requests();
        assert_eq!(requests.len(), 1, "one create-reaction request");
        assert_eq!(requests[0].method, HttpMethod::Post);
        assert_eq!(
            requests[0].url,
            "https://open.feishu.cn/open-apis/im/v1/messages/om_1/reactions"
        );
        assert!(
            requests[0]
                .headers
                .iter()
                .any(|(name, value)| name == "Authorization" && value == "Bearer t-tenant"),
            "the reaction must carry the tenant token: {:?}",
            requests[0].headers
        );
        // The documented body: `reaction_type.emoji_type`, and no chat id — the endpoint is
        // message-scoped.
        assert_eq!(
            request_body(&requests[0]),
            json!({ "reaction_type": { "emoji_type": "Typing" } })
        );

        // The opaque handle is cached under exactly (message_id, emoji): delete needs it.
        assert_eq!(
            client.cached_reaction_id("om_1", REACTION_IN_PROGRESS),
            Some("re_1".to_string())
        );
        assert_eq!(client.cached_reaction_id("om_1", REACTION_FAILURE), None);
        assert_eq!(
            client.cached_reaction_id("om_2", REACTION_IN_PROGRESS),
            None
        );
        assert_eq!(client.cached_reaction_id("om_1", "👍"), None);
    }

    #[tokio::test]
    async fn remove_reaction_deletes_the_cached_id_and_drops_it() {
        // Added with padded input, removed with the plain key: both normalize to one cache key.
        let mock = MockTransport::new(vec![
            token_ok(7200),
            reaction_created("re_7"),
            reaction_deleted(),
        ]);
        let client = adapter(&mock);

        assert!(client.add_reaction("oc_chat", "om_1", "  Typing  ").await);
        assert!(client.remove_reaction("oc_chat", "om_1", "Typing").await);

        let requests = mock.api_requests();
        assert_eq!(requests.len(), 2);
        assert_eq!(requests[1].method, HttpMethod::Delete);
        assert_eq!(
            requests[1].url,
            "https://open.feishu.cn/open-apis/im/v1/messages/om_1/reactions/re_7"
        );
        assert!(
            requests[1].body.is_none(),
            "a reaction delete carries no body: {:?}",
            requests[1].body
        );
        assert!(
            requests[1]
                .headers
                .iter()
                .any(|(name, value)| name == "Authorization" && value == "Bearer t-tenant"),
            "the delete must carry the tenant token: {:?}",
            requests[1].headers
        );

        // A confirmed delete dropped the handle, so a second remove refuses locally: no request.
        assert_eq!(
            client.cached_reaction_id("om_1", REACTION_IN_PROGRESS),
            None
        );
        assert!(!client.remove_reaction("oc_chat", "om_1", "Typing").await);
        assert_eq!(mock.api_requests().len(), 2, "no third request");
        assert_eq!(mock.token_requests(), 1, "the token was reused");
    }

    #[tokio::test]
    async fn remove_reaction_without_a_cached_id_makes_no_request() {
        let mock = MockTransport::new(Vec::new());
        // No add ever happened, so there is no handle: a guessed id is never deleted.
        assert!(
            !adapter(&mock)
                .remove_reaction("oc_chat", "om_1", REACTION_IN_PROGRESS)
                .await
        );
        assert!(
            mock.requests().is_empty(),
            "an unknown handle must not reach the wire, token included"
        );
    }

    #[tokio::test]
    async fn a_failed_delete_keeps_the_handle_for_a_retry() {
        let mock = MockTransport::new(vec![
            token_ok(7200),
            reaction_created("re_keep"),
            feishu_error(502, 100_000, "bad gateway"),
            Err(TransportError::Request("connection reset".to_string())),
            reaction_deleted(),
        ]);
        let client = adapter(&mock);
        assert!(
            client
                .add_reaction("oc_chat", "om_1", REACTION_IN_PROGRESS)
                .await
        );

        // A 5xx and then a transport failure: both report `false` and both keep the handle, so a
        // retry still knows which id to delete (hermes keeps it too, adapter.py:2614-2617).
        assert!(
            !client
                .remove_reaction("oc_chat", "om_1", REACTION_IN_PROGRESS)
                .await
        );
        assert_eq!(
            client.cached_reaction_id("om_1", REACTION_IN_PROGRESS),
            Some("re_keep".to_string())
        );
        assert!(
            !client
                .remove_reaction("oc_chat", "om_1", REACTION_IN_PROGRESS)
                .await
        );
        assert_eq!(
            client.cached_reaction_id("om_1", REACTION_IN_PROGRESS),
            Some("re_keep".to_string())
        );

        // The retry deletes the very same id, and only the confirmed delete drops it.
        assert!(
            client
                .remove_reaction("oc_chat", "om_1", REACTION_IN_PROGRESS)
                .await
        );
        let deletes: Vec<String> = mock
            .api_requests()
            .into_iter()
            .filter(|request| request.method == HttpMethod::Delete)
            .map(|request| request.url)
            .collect();
        assert_eq!(deletes.len(), 3);
        assert_eq!(
            deletes[0],
            "https://open.feishu.cn/open-apis/im/v1/messages/om_1/reactions/re_keep"
        );
        assert_eq!(deletes[1], deletes[0], "a retry reuses the cached id");
        assert_eq!(deletes[2], deletes[0]);
        assert_eq!(
            client.cached_reaction_id("om_1", REACTION_IN_PROGRESS),
            None
        );
    }

    #[tokio::test]
    async fn a_repeat_add_resends_and_replaces_the_handle() {
        // The reference's skip-if-cached guard is caller state (`on_processing_start`,
        // adapter.py:2598); this adapter re-sends and keeps the newest id rather than answering
        // `true` from a handle that may no longer be on the message.
        let mock = MockTransport::new(vec![
            token_ok(7200),
            reaction_created("re_first"),
            reaction_created("re_second"),
            reaction_deleted(),
        ]);
        let client = adapter(&mock);

        assert!(
            client
                .add_reaction("oc_chat", "om_1", REACTION_IN_PROGRESS)
                .await
        );
        assert!(
            client
                .add_reaction("oc_chat", "om_1", REACTION_IN_PROGRESS)
                .await
        );
        assert_eq!(mock.api_requests().len(), 2);
        assert_eq!(
            client.cached_reaction_id("om_1", REACTION_IN_PROGRESS),
            Some("re_second".to_string())
        );

        assert!(
            client
                .remove_reaction("oc_chat", "om_1", REACTION_IN_PROGRESS)
                .await
        );
        let requests = mock.api_requests();
        assert!(
            requests[2].url.ends_with("/om_1/reactions/re_second"),
            "the newest handle is the one deleted: {}",
            requests[2].url
        );
    }

    #[tokio::test]
    async fn a_rejected_add_caches_nothing_and_claims_no_reaction() {
        // Feishu refuses a glyph it has no key for; the adapter reports a failure.
        let mock = MockTransport::new(vec![
            token_ok(7200),
            feishu_error(400, 231_009, "emoji not allowed"),
        ]);
        let client = adapter(&mock);

        assert!(!client.add_reaction("oc_chat", "om_1", "👍").await);
        assert_eq!(client.cached_reaction_id("om_1", "👍"), None);
        // With nothing cached a remove is refused locally: only the failed add reached the wire.
        assert!(!client.remove_reaction("oc_chat", "om_1", "👍").await);
        assert_eq!(mock.api_requests().len(), 1);
        assert_eq!(mock.token_requests(), 1);
    }

    #[tokio::test]
    async fn a_code_zero_add_without_a_reaction_id_is_not_a_reaction() {
        // HTTP 200 + `code: 0` but no `data.reaction_id`: nothing could be deleted later, so the
        // call is not reported as a reaction.
        let no_id = MockTransport::new(vec![
            token_ok(7200),
            response(200, json!({ "code": 0, "msg": "success", "data": {} })),
        ]);
        let client = adapter(&no_id);
        assert!(
            !client
                .add_reaction("oc_chat", "om_1", REACTION_IN_PROGRESS)
                .await
        );
        assert_eq!(
            client.cached_reaction_id("om_1", REACTION_IN_PROGRESS),
            None
        );

        // A blank id is no handle either.
        let blank_id = MockTransport::new(vec![
            token_ok(7200),
            response(
                200,
                json!({ "code": 0, "msg": "success", "data": { "reaction_id": "   " } }),
            ),
        ]);
        assert!(
            !adapter(&blank_id)
                .add_reaction("oc_chat", "om_1", REACTION_IN_PROGRESS)
                .await
        );
    }

    #[tokio::test]
    async fn reaction_emoji_is_passed_through_like_the_reference() {
        // The reference accepts Feishu keys only and sends them verbatim (adapter.py:2572-2583):
        // trimming is the one normalization, and a glyph is not rewritten into a key.
        assert_eq!(
            reaction_emoji_type("Typing"),
            Some(REACTION_IN_PROGRESS.to_string())
        );
        assert_eq!(
            reaction_emoji_type("  CrossMark  "),
            Some(REACTION_FAILURE.to_string())
        );
        assert_eq!(reaction_emoji_type("👍"), Some("👍".to_string()));
        assert_eq!(reaction_emoji_type("   "), None);
        assert_eq!(reaction_emoji_type(""), None);
        assert_eq!(reaction_emoji_type("\n\t"), None);

        // On the wire the trimmed value lands in `reaction_type.emoji_type`, unchanged.
        let mock = MockTransport::new(vec![token_ok(7200), reaction_created("re_glyph")]);
        assert!(
            adapter(&mock)
                .add_reaction("oc_chat", "om_1", "  👍  ")
                .await
        );
        assert_eq!(
            request_body(&mock.api_requests()[0]),
            json!({ "reaction_type": { "emoji_type": "👍" } })
        );
        assert!(
            !request_body(&mock.api_requests()[0])
                .to_string()
                .contains("Typing"),
            "no glyph->key table is invented"
        );
    }

    #[tokio::test]
    async fn blank_reaction_inputs_are_refused_with_zero_requests() {
        let mock = MockTransport::new(Vec::new());
        let client = adapter(&mock);
        let cases = [
            ("", "om_1", REACTION_IN_PROGRESS),
            ("   ", "om_1", REACTION_IN_PROGRESS),
            ("\n\t", "om_1", REACTION_IN_PROGRESS),
            ("oc_chat", "", REACTION_IN_PROGRESS),
            ("oc_chat", "  ", REACTION_IN_PROGRESS),
            ("oc_chat", "om_1", ""),
            ("oc_chat", "om_1", "  \n\t "),
        ];
        for (chat_id, message_id, emoji) in cases {
            assert!(
                !client.add_reaction(chat_id, message_id, emoji).await,
                "add {chat_id:?}/{message_id:?}/{emoji:?}"
            );
            assert!(
                !client.remove_reaction(chat_id, message_id, emoji).await,
                "remove {chat_id:?}/{message_id:?}/{emoji:?}"
            );
        }
        assert!(
            mock.requests().is_empty(),
            "blank inputs must not reach the wire, token fetch included"
        );
    }

    #[tokio::test]
    async fn reaction_failures_keep_the_send_taxonomy() {
        // (status, code, msg, expected kind, retryable)
        let cases = [
            (403, 0, "no permission", "forbidden", false),
            (403, 99_991_663, "Invalid access token", "forbidden", false),
            (404, 0, "not found", "not_found", false),
            (400, 230_002, "chat not found", "not_found", false),
            (400, 100_002, "invalid parameter", "bad_format", false),
            (429, 0, "too many requests", "rate_limited", true),
            (400, 230_020, "frequency limit", "rate_limited", true),
            (502, 100_000, "bad gateway", "transient", true),
            // A 2xx envelope carrying a non-zero code is a failure too.
            (200, 231_009, "emoji not allowed", "transient", true),
        ];
        for (status, code, msg, kind, retryable) in cases {
            let mock = MockTransport::new(vec![token_ok(7200), feishu_error(status, code, msg)]);
            let client = adapter(&mock);
            let request = add_reaction_request(&client, "om_1", REACTION_IN_PROGRESS);
            let Err(failure) = client.reaction_request(request, "reaction add").await else {
                panic!("HTTP {status} code {code} must fail");
            };
            assert_eq!(failure.kind, kind, "HTTP {status} code {code}");
            assert_eq!(failure.retryable, retryable, "HTTP {status} code {code}");
            assert!(
                failure.message.contains(msg),
                "HTTP {status} code {code} lost the message: {:?}",
                failure.message
            );
        }

        // The public call turns every classification into `false`, never a claimed reaction.
        for (status, code, msg, _, _) in cases {
            let mock = MockTransport::new(vec![token_ok(7200), feishu_error(status, code, msg)]);
            let client = adapter(&mock);
            assert!(
                !client
                    .add_reaction("oc_chat", "om_1", REACTION_IN_PROGRESS)
                    .await,
                "HTTP {status} code {code} must report false"
            );
            assert_eq!(
                client.cached_reaction_id("om_1", REACTION_IN_PROGRESS),
                None
            );
        }

        // A transport failure on the add is a typed transient failure, with nothing cached. Two
        // scripted failures: `add_reaction` and the direct call below each take one.
        let down = MockTransport::new(vec![
            token_ok(7200),
            Err(TransportError::Request("connection reset".to_string())),
            Err(TransportError::Request("connection reset".to_string())),
        ]);
        let client = adapter(&down);
        assert!(
            !client
                .add_reaction("oc_chat", "om_1", REACTION_IN_PROGRESS)
                .await
        );
        let request = add_reaction_request(&client, "om_1", REACTION_IN_PROGRESS);
        match client.reaction_request(request, "reaction add").await {
            Ok(_) => panic!("a transport failure must not succeed"),
            Err(failure) => {
                assert_eq!(failure.kind, "transient");
                assert!(failure.retryable);
                assert!(
                    failure.message.contains("reaction add transport error"),
                    "{:?}",
                    failure.message
                );
                assert!(
                    failure.message.contains("connection reset"),
                    "{:?}",
                    failure.message
                );
            }
        }
    }

    #[tokio::test]
    async fn a_rate_limited_reaction_honours_retry_after() {
        let mock = MockTransport::new(vec![
            token_ok(7200),
            response_with_retry_after(429, json!({ "code": 230020, "msg": "slow down" }), 9.0),
        ]);
        let client = adapter(&mock);
        let request = add_reaction_request(&client, "om_1", REACTION_IN_PROGRESS);
        let Err(failure) = client.reaction_request(request, "reaction add").await else {
            panic!("a 429 must fail");
        };
        assert_eq!(failure.kind, "rate_limited");
        assert!(failure.retryable);
        assert_eq!(failure.retry_after, Some(9.0));
    }

    #[tokio::test]
    async fn a_reaction_token_error_drops_the_cached_token() {
        let mock = MockTransport::new(vec![
            token_ok(7200),
            feishu_error(401, 99_991_663, "Invalid access token"),
            token_ok(7200),
            reaction_created("re_after"),
        ]);
        let client = adapter(&mock);

        assert!(
            !client
                .add_reaction("oc_chat", "om_1", REACTION_IN_PROGRESS)
                .await
        );
        // The stale token was thrown away, so the next call fetches a fresh one and succeeds.
        assert!(
            client
                .add_reaction("oc_chat", "om_1", REACTION_IN_PROGRESS)
                .await
        );
        assert_eq!(mock.token_requests(), 2);
        assert_eq!(
            client.cached_reaction_id("om_1", REACTION_IN_PROGRESS),
            Some("re_after".to_string())
        );
    }

    #[tokio::test]
    async fn reaction_urls_percent_encode_their_path_segments() {
        let mock = MockTransport::new(vec![
            token_ok(7200),
            reaction_created("re/1"),
            reaction_deleted(),
        ]);
        let client = adapter(&mock);

        assert!(
            client
                .add_reaction("oc_chat", "om_1/x", REACTION_IN_PROGRESS)
                .await
        );
        assert!(
            client
                .remove_reaction("oc_chat", "om_1/x", REACTION_IN_PROGRESS)
                .await
        );
        let requests = mock.api_requests();
        assert_eq!(
            requests[0].url,
            "https://open.feishu.cn/open-apis/im/v1/messages/om_1%2Fx/reactions"
        );
        assert_eq!(
            requests[1].url,
            "https://open.feishu.cn/open-apis/im/v1/messages/om_1%2Fx/reactions/re%2F1",
            "the returned id is encoded as one path segment"
        );
    }

    #[test]
    fn the_reaction_cache_is_bounded_and_ordered() {
        let mut cache = ReactionCache::default();
        for index in 0..REACTION_CACHE_CAPACITY + 2 {
            cache.store(
                &format!("om_{index}"),
                REACTION_IN_PROGRESS,
                &format!("re_{index}"),
            );
        }
        assert_eq!(cache.ids.len(), REACTION_CACHE_CAPACITY);
        // The two oldest handles were evicted first; the newest are intact.
        assert_eq!(cache.get("om_0", REACTION_IN_PROGRESS), None);
        assert_eq!(cache.get("om_1", REACTION_IN_PROGRESS), None);
        assert_eq!(
            cache.get("om_2", REACTION_IN_PROGRESS),
            Some("re_2".to_string())
        );
        let newest = format!("om_{}", REACTION_CACHE_CAPACITY + 1);
        assert_eq!(
            cache.get(&newest, REACTION_IN_PROGRESS),
            Some(format!("re_{}", REACTION_CACHE_CAPACITY + 1))
        );

        // A replace keeps the slot and stores the newest handle; a remove drops key and queue entry.
        cache.store("om_2", REACTION_IN_PROGRESS, "re_2b");
        assert_eq!(
            cache.get("om_2", REACTION_IN_PROGRESS),
            Some("re_2b".to_string())
        );
        assert_eq!(cache.ids.len(), REACTION_CACHE_CAPACITY);
        cache.remove("om_2", REACTION_IN_PROGRESS);
        assert_eq!(cache.get("om_2", REACTION_IN_PROGRESS), None);
        assert!(
            !cache
                .order
                .contains(&("om_2".to_string(), REACTION_IN_PROGRESS.to_string())),
            "a removed key must not linger in the eviction queue"
        );

        // The key is the pair, so each emoji keeps its own handle.
        let mut cache = ReactionCache::default();
        cache.store("om_1", REACTION_IN_PROGRESS, "re_start");
        cache.store("om_1", REACTION_FAILURE, "re_fail");
        assert_eq!(
            cache.get("om_1", REACTION_IN_PROGRESS),
            Some("re_start".to_string())
        );
        assert_eq!(
            cache.get("om_1", REACTION_FAILURE),
            Some("re_fail".to_string())
        );
        assert_eq!(cache.get("om_1", "👍"), None);
    }
}
