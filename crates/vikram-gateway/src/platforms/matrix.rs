//! Matrix adapter — client-server API (`/_matrix/client/v3`).
//!
//! Port of `gateway/platforms/matrix` from hermes-agent, reduced to the client-server API itself.
//! hermes rides on the `mautrix` SDK (full sync loop, E2EE/olm, crypto store, invites, threads,
//! reactions, media upload). This port talks the documented HTTP endpoints directly through the
//! frozen [`Transport`](crate::transport::Transport) — build an [`HttpRequest`], hand it over,
//! classify the response — so no Matrix SDK (and no new dependency) is involved.
//!
//! Ported: token auth probe (`GET /account/whoami`), event send with the 8000-code-point budget
//! (`truncate_message`), reply relations, edit-as-replace (`m.relates_to.m.replace`), room name /
//! canonical-alias lookup, one-shot `GET /sync` long-poll plus event normalization, the best-effort
//! typing indicator, outbound media (`POST /media/v3/upload` then an `m.room.message` send), and
//! reactions (`m.reaction` annotation sends plus the redaction that removes one).
//!
//! Fidelity notes (full accounting in docs/gateway-port-plan.md):
//! - **Request verbs.** Room writes are real `PUT`s (`HttpRequest::put_json`); the transport now
//!   exposes `Get`/`Post`/`Put`/`Patch`/`Delete`.
//! - hermes serves the HTML `formatted_body` (`markdown` → `org.matrix.custom.html`). Rendering
//!   markdown to Matrix HTML needs a markdown engine this dependency-free port does not have, so
//!   only the plain `m.text` body is sent; Matrix renders the plain body everywhere.
//! - hermes' E2EE stack (olm/mautrix, key verification, cross-signing), encrypted-attachment
//!   uploads (`encrypt_attachment`, `file` instead of `url` in the content), inbound media download
//!   (the mxc body is reported as a MIME type only), invite handling, `m.direct` DM detection,
//!   threads, and inbound reaction handling (approval/model/choice pickers, reactions arriving
//!   through `/sync`) are not ported. Rooms are reported as `"group"` ([`ChatInfo`] documents
//!   deducing a DM from member counts or the `m.direct` account data, neither of which is available
//!   here).
//! - **A reaction is an annotation event, and removing one is a redaction.** hermes sends
//!   `m.reaction` with `m.relates_to: {rel_type: "m.annotation", event_id, key}` and keeps the
//!   returned reaction event id, because Matrix has no "unreact" call: the reaction is withdrawn by
//!   redacting the bot's own reaction event (`_send_reaction` → `_redact_reaction` →
//!   `redact_message`, adapter.py:2443-2457 / 2708-2711). This port keeps the same per-annotation
//!   event cache, keyed by `(room, target event, emoji)`, and refuses to redact when no reaction
//!   event id is known.
//! - **Outbound media is two steps, like hermes.** `send_media` uploads the bytes to
//!   `POST /_matrix/media/v3/upload?filename=…` (Content-Type from the MIME), takes the returned
//!   `content_uri`, and only then sends the `m.room.message` that carries it. An upload failure
//!   returns before any send is attempted, and a send failure is never reported as a delivery.
//!   `MediaSource::Url` is refused: hermes downloads an external http(s) URL itself under an SSRF
//!   guard and a `MATRIX_MAX_MEDIA_BYTES` cap (`send_image` → `_download_external_media_with_cap`),
//!   and its inbound path refuses any `url` that is not `mxc://`
//!   (`_handle_media_message`) — this port has no fetcher, so it tells the caller to supply bytes
//!   instead of pretending a URL went out.
//! - hermes measures the budget in UTF-8 bytes (`MATRIX_MAX_MESSAGE_LENGTH`, default 15000) because
//!   homeservers cap event *bytes*. This port measures code points against
//!   [`MAX_MESSAGE_LENGTH`], the budget this adapter's trait surface advertises.
//! - `poll_once` skips events whose sender is our own user id (echo-loop guard) and drops
//!   `m.relates_to.rel_type = "m.replace"` edits, whose original already went through the pipeline.
//!   hermes additionally drops *every* event when its own user id is unresolved; this port only
//!   skips on a proven match, so `connect` should still run before the first `poll_once`.

use std::collections::HashMap;
use std::sync::Mutex;
use std::sync::atomic::{AtomicU64, Ordering};

use async_trait::async_trait;
use serde_json::{Map, Value, json};

use crate::event::{MessageEvent, MessageType, Platform, SessionSource, platform_names};
use crate::helpers::{codepoint_len, truncate_message};
use crate::platform::{
    ChatInfo, MediaKind, MediaSource, OutboundMedia, PlatformAdapter, SendResult,
};
use crate::transport::{HttpRequest, HttpResponse, SharedTransport, TransportError};

/// Path prefix every client-server API call hangs off (appended to the homeserver URL).
pub const MATRIX_API_PREFIX: &str = "/_matrix/client/v3";

/// Path prefix the media repository hangs off. Uploads are *not* on the client-server prefix:
/// hermes' `client.upload_media(...)` reaches `/_matrix/media/v3/upload` (mautrix `HTTPAPI`).
const MATRIX_MEDIA_API_PREFIX: &str = "/_matrix/media/v3";

/// Largest attachment this adapter uploads, hermes' `MATRIX_MAX_MEDIA_BYTES` default (100 MiB).
///
/// hermes checks the size before uploading (`_upload_and_send` → `_media_too_large`, "Media file
/// exceeds Matrix limit (N > max bytes)"); the homeserver caps the same number, so a request that
/// big would only come back as `M_TOO_LARGE`. Refusing locally keeps the bytes off the wire.
pub const MAX_MEDIA_BYTES: usize = 100 * 1024 * 1024;

/// This adapter's per-event text budget, in Unicode scalar values. `send` splits to it with
/// [`truncate_message`] and [`PlatformAdapter::max_message_length`] reports it.
pub const MAX_MESSAGE_LENGTH: usize = 8000;

/// Typing timeout advertised to the homeserver, in milliseconds (the spec's maximum is 60 s).
const TYPING_TIMEOUT_MS: u32 = 30_000;

/// Characters of an error body kept in a [`SendResult::error`] / `Err` message.
const BODY_EXCERPT_CHARS: usize = 200;

/// `m.room.message` msgtype for plain text; the only msgtype this adapter sends.
const MSGTYPE_TEXT: &str = "m.text";

/// Per-process instance sequence, so two adapters in one process never mint the same transaction
/// id (Matrix deduplicates `PUT …/send/…/{txn_id}` per access token, which would drop a chunk).
static INSTANCE_SEQ: AtomicU64 = AtomicU64::new(0);

/// Matrix adapter speaking the client-server API with an access token.
///
/// `connect` resolves our own user id (needed to skip our echoed events and to address the typing
/// endpoint); every other method works without it.
pub struct MatrixAdapter {
    transport: SharedTransport,
    homeserver: String,
    access_token: String,
    /// Absolute API base: `{homeserver}/_matrix/client/v3`, unless `with_api_base` overrode it.
    api_base: String,
    /// Our own Matrix user id (`@bot:server`), resolved by [`connect`](MatrixAdapter::connect).
    user_id: Option<String>,
    /// Monotonic transaction-id counter; see [`MatrixAdapter::txn_id`].
    txn_seq: AtomicU64,
    /// Process-unique transaction-id prefix (`vikram-{pid}-{instance}`).
    txn_prefix: String,
    /// Reaction event ids we sent, keyed by `(room_id, target event_id, emoji key)`.
    ///
    /// Matrix withdraws a reaction by redacting the event that carried it, so the id of *our*
    /// `m.reaction` event is the only handle on the annotation. hermes keeps the same fact in
    /// `_pending_reactions` / `bot_reaction_events` (`dict[str, str]` of emoji → event id,
    /// adapter.py:355 / 874) and then calls `redact_message` on it (adapter.py:2456-2457).
    reaction_events: Mutex<HashMap<(String, String, String), String>>,
}

impl MatrixAdapter {
    /// Build an adapter over `transport` for `homeserver` (`https://matrix.example.org`) with an
    /// access token.
    ///
    /// A trailing `/` on `homeserver` is trimmed; nothing is sent anywhere until a method runs.
    pub fn new(
        transport: SharedTransport,
        homeserver: impl Into<String>,
        access_token: impl Into<String>,
    ) -> Self {
        let homeserver = homeserver.into().trim().trim_end_matches('/').to_string();
        let api_base = format!("{homeserver}{MATRIX_API_PREFIX}");
        Self {
            transport,
            homeserver,
            access_token: access_token.into(),
            api_base,
            user_id: None,
            txn_seq: AtomicU64::new(0),
            txn_prefix: format!(
                "vikram-{}-{}",
                std::process::id(),
                INSTANCE_SEQ.fetch_add(1, Ordering::Relaxed)
            ),
            reaction_events: Mutex::new(HashMap::new()),
        }
    }

    /// Point the adapter at an absolute API base instead of `{homeserver}/_matrix/client/v3`.
    ///
    /// Only useful for tests (a mock server URL) or a homeserver published under a path prefix.
    pub fn with_api_base(mut self, api_base: impl Into<String>) -> Self {
        self.api_base = api_base.into().trim_end_matches('/').to_string();
        self
    }

    /// The homeserver this adapter authenticates against, without a trailing slash.
    pub fn homeserver(&self) -> &str {
        &self.homeserver
    }

    /// Our own Matrix user id, once [`connect`](PlatformAdapter::connect) resolved it.
    pub fn user_id(&self) -> Option<&str> {
        self.user_id.as_deref()
    }

    /// Absolute URL for an API path such as `/account/whoami`.
    fn endpoint(&self, path: &str) -> String {
        format!("{}{path}", self.api_base)
    }

    /// Add the access token every client-server API call needs.
    fn authorized(&self, request: HttpRequest) -> HttpRequest {
        request.bearer(&self.access_token)
    }

    /// A room write: JSON body, bearer auth, and the `PUT` verb the API requires.
    fn put_json(&self, path: &str, body: &Value) -> HttpRequest {
        self.authorized(HttpRequest::put_json(self.endpoint(path), body))
    }

    /// A transaction id unique within this process and across restarts: `{prefix}-{counter}`.
    ///
    /// Matrix deduplicates `PUT …/send/{type}/{txn_id}`, so two chunks must never share one.
    fn txn_id(&self) -> String {
        format!(
            "{}-{}",
            self.txn_prefix,
            self.txn_seq.fetch_add(1, Ordering::Relaxed)
        )
    }

    /// Path of one `m.room.message` send into `room_id`.
    fn send_event_path(&self, room_id: &str, txn_id: &str) -> String {
        format!(
            "/rooms/{}/send/m.room.message/{txn_id}",
            percent_encode(room_id)
        )
    }

    /// Path of one `m.reaction` send into `room_id`.
    ///
    /// The event type is part of the path (`PUT /rooms/{roomId}/send/{eventType}/{txnId}`), which is
    /// how hermes' `send_message_event(RoomID(room_id), EventType.REACTION, content)` reaches the
    /// homeserver (adapter.py:2449).
    fn reaction_event_path(&self, room_id: &str, txn_id: &str) -> String {
        format!(
            "/rooms/{}/send/m.reaction/{txn_id}",
            percent_encode(room_id)
        )
    }

    /// Path of the redaction of one event: `PUT /rooms/{roomId}/redact/{eventId}/{txnId}`.
    ///
    /// hermes reaches it through `self._client.redact(RoomID(room_id), EventID(event_id),
    /// reason=…)` (adapter.py:2710), which is what `_redact_reaction` uses to withdraw a reaction.
    fn redact_event_path(&self, room_id: &str, event_id: &str, txn_id: &str) -> String {
        format!(
            "/rooms/{}/redact/{}/{txn_id}",
            percent_encode(room_id),
            percent_encode(event_id)
        )
    }

    /// Which reaction event our annotation became, for a later removal.
    ///
    /// Same fact the reference stores (`bot_reaction_events[emoji] = event_id`, adapter.py:1665):
    /// without it the bot could only guess which event to redact.
    fn remember_reaction(&self, room_id: &str, event_id: &str, key: &str, reaction_event_id: &str) {
        let mut events = self
            .reaction_events
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner());
        events.insert(
            (room_id.to_string(), event_id.to_string(), key.to_string()),
            reaction_event_id.to_string(),
        );
    }

    /// The reaction event id recorded for `(room, event, key)`, if any.
    fn stored_reaction(&self, room_id: &str, event_id: &str, key: &str) -> Option<String> {
        let events = self
            .reaction_events
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner());
        events
            .get(&(room_id.to_string(), event_id.to_string(), key.to_string()))
            .cloned()
    }

    /// Forget a reaction that is no longer on the message.
    fn forget_reaction(&self, room_id: &str, event_id: &str, key: &str) {
        let mut events = self
            .reaction_events
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner());
        events.remove(&(room_id.to_string(), event_id.to_string(), key.to_string()));
    }

    /// Send one `m.reaction` annotation and report the event id it became.
    ///
    /// Content is the reference's exactly — `m.relates_to: {rel_type: "m.annotation", event_id,
    /// key}` (adapter.py:2447) — with `event_id` naming the reacted-to event and `key` the emoji
    /// itself (Matrix keys are glyphs, not names).
    async fn send_reaction(
        &self,
        room_id: &str,
        event_id: &str,
        key: &str,
    ) -> Result<String, Box<SendResult>> {
        let path = self.reaction_event_path(room_id, &self.txn_id());
        self.put_event(&path, &reaction_payload(event_id, key))
            .await
    }

    /// Redact one of our reaction events, withdrawing the annotation.
    ///
    /// The body is the reference's: `redact_message(…, reason="")` passes `reason=None`
    /// (adapter.py:2456-2457 / 2708-2711), so no `reason` field is sent at all. The reply's
    /// `event_id` is the redaction event, which is what [`put_event`](Self::put_event) reports.
    async fn redact_reaction(
        &self,
        room_id: &str,
        reaction_event_id: &str,
    ) -> Result<String, Box<SendResult>> {
        let path = self.redact_event_path(room_id, reaction_event_id, &self.txn_id());
        self.put_event(&path, &json!({})).await
    }

    /// Execute one room write and return the event id it created.
    ///
    /// Errors carry the classified [`SendResult`] (boxed: the success path stays small).
    async fn put_event(&self, path: &str, body: &Value) -> Result<String, Box<SendResult>> {
        self.put_event_classified(path, body, classify_failure)
            .await
    }

    /// [`put_event`](Self::put_event) with an explicit classifier, so the media path can map the
    /// upload-specific statuses (413 / `M_TOO_LARGE` → `too_long`) without changing text sends.
    async fn put_event_classified(
        &self,
        path: &str,
        body: &Value,
        classify: fn(&HttpResponse) -> SendResult,
    ) -> Result<String, Box<SendResult>> {
        let request = self.put_json(path, body);
        let response = self
            .transport
            .execute(request)
            .await
            .map_err(|err| Box::new(transport_failure(&err)))?;
        if !response.ok() {
            return Err(Box::new(classify(&response)));
        }
        match response.json() {
            Ok(decoded) => match event_id_from(&decoded) {
                Some(event_id) => Ok(event_id),
                // Delivered but unidentified: report it, never resend blindly.
                None => Err(Box::new(SendResult::failed(
                    "matrix send returned no event_id",
                    Some("unknown"),
                ))),
            },
            Err(err) => Err(Box::new(SendResult::failed(
                format!("matrix send response was not json: {err}"),
                Some("unknown"),
            ))),
        }
    }

    /// Absolute base of the media repository, derived from the configured API base.
    ///
    /// Normally `{homeserver}/_matrix/media/v3`. [`with_api_base`](MatrixAdapter::with_api_base)
    /// publishes a client-server base, so its origin is reused (a mock server or a homeserver under
    /// a path prefix reaches its media repository on the same host); an API base that is not the
    /// client-server prefix falls back to the homeserver URL.
    fn media_base(&self) -> String {
        match self.api_base.strip_suffix(MATRIX_API_PREFIX) {
            Some(prefix) => format!("{prefix}{MATRIX_MEDIA_API_PREFIX}"),
            None => format!("{}{MATRIX_MEDIA_API_PREFIX}", self.homeserver),
        }
    }

    /// Upload `data` as `filename` and return the `content_uri` the homeserver minted.
    ///
    /// `POST {media}/upload?filename=…` with the MIME as `Content-Type` and the raw bytes as the
    /// body — hermes' `client.upload_media(upload_data, mime_type=…, filename=…, size=…)`, whose
    /// mautrix implementation posts the same query and header. Errors carry the classified
    /// [`SendResult`]; the caller must not send a message after one.
    async fn upload_media(
        &self,
        data: &[u8],
        filename: &str,
        mime: &str,
    ) -> Result<String, Box<SendResult>> {
        let url = format!(
            "{}/upload?filename={}",
            self.media_base(),
            percent_encode(filename)
        );
        let request = self
            .authorized(HttpRequest::post(url))
            .header("Content-Type", mime)
            .body(data);
        let response = self
            .transport
            .execute(request)
            .await
            .map_err(|err| Box::new(transport_failure(&err)))?;
        if !response.ok() {
            return Err(Box::new(classify_media_failure(&response)));
        }
        let decoded = match response.json() {
            Ok(decoded) => decoded,
            Err(err) => {
                return Err(Box::new(SendResult::failed(
                    format!("matrix upload response was not json: {err}"),
                    Some("unknown"),
                )));
            }
        };
        match content_uri_from(&decoded) {
            // Uploaded but unusable: report it, never send a message with no media in it.
            None => Err(Box::new(SendResult::failed(
                "matrix upload returned no content_uri",
                Some("unknown"),
            ))),
            Some(content_uri) => Ok(content_uri),
        }
    }

    /// Upload `media`'s bytes and send the `m.room.message` that carries them.
    ///
    /// The two steps are hermes' `_upload_and_send`: upload first (any failure ends the call with a
    /// typed failure and no send attempt), then one event whose content is
    /// `{msgtype, body, url, info{mimetype, size}}` plus the reply relation when `reply_to` is set.
    async fn send_media_event(
        &self,
        chat_id: &str,
        media: &OutboundMedia,
        data: &[u8],
        reply_to: Option<&str>,
    ) -> SendResult {
        let filename = media_filename(media);
        let mime = media_mime(media, &filename);
        let content_uri = match self.upload_media(data, &filename, &mime).await {
            Ok(content_uri) => content_uri,
            Err(failure) => return *failure,
        };
        let payload = media_payload(
            msgtype_for(media.kind),
            media_body(media, &filename).as_str(),
            &content_uri,
            &mime,
            data.len(),
            reply_to,
        );
        let path = self.send_event_path(chat_id, &self.txn_id());
        match self
            .put_event_classified(&path, &payload, classify_media_failure)
            .await
        {
            Ok(event_id) => SendResult::ok(event_id),
            Err(failure) => *failure,
        }
    }

    /// Read one string field out of a room state event (`GET /rooms/{room}/state/{type}`).
    ///
    /// Any failure — transport, non-2xx, unparseable body, missing or blank field — is `None`, so
    /// callers can chain fallbacks without distinguishing 404 from "no name set".
    async fn room_state_string(
        &self,
        room_id: &str,
        event_type: &str,
        key: &str,
    ) -> Option<String> {
        let path = format!("/rooms/{}/state/{event_type}", percent_encode(room_id));
        let request = self.authorized(HttpRequest::get(self.endpoint(&path)));
        let response = self.transport.execute(request).await.ok()?;
        if !response.ok() {
            return None;
        }
        let body = response.json().ok()?;
        body.get(key)
            .and_then(Value::as_str)
            .map(str::trim)
            .filter(|value| !value.is_empty())
            .map(str::to_string)
    }

    /// Long-poll `GET /sync` once and normalize the room messages it carries.
    ///
    /// `since` is the `next_batch` token from the previous call (`None` for the initial sync) and
    /// `timeout_ms` is the server-side hold. The returned tuple is `(events, next_batch)`; the
    /// token is `None` only when the homeserver omitted it.
    ///
    /// Only `rooms.join.*.timeline.events` of type `m.room.message` with a supported msgtype become
    /// events. Events our own user id sent, and `m.replace` edits, are skipped. Transport errors,
    /// non-2xx statuses and unparseable JSON are reported as `Err` so a caller can back off.
    pub async fn poll_once(
        &self,
        since: Option<&str>,
        timeout_ms: u32,
    ) -> Result<(Vec<MessageEvent>, Option<String>), String> {
        let mut url = format!("{}?timeout={timeout_ms}", self.endpoint("/sync"));
        if let Some(token) = since {
            url.push_str("&since=");
            url.push_str(&percent_encode(token));
        }

        let response = self
            .transport
            .execute(self.authorized(HttpRequest::get(url)))
            .await
            .map_err(|err| format!("matrix sync transport error: {err}"))?;
        if !response.ok() {
            return Err(format!(
                "matrix sync failed with status {}: {}",
                response.status,
                body_excerpt(&response.body)
            ));
        }
        let parsed: Value = serde_json::from_str(&response.body)
            .map_err(|err| format!("matrix sync returned invalid json: {err}"))?;

        let next_batch = parsed
            .get("next_batch")
            .and_then(Value::as_str)
            .map(str::trim)
            .filter(|token| !token.is_empty())
            .map(str::to_string);

        let mut events = Vec::new();
        let joined = parsed
            .get("rooms")
            .and_then(|rooms| rooms.get("join"))
            .and_then(Value::as_object);
        for (room_id, room) in joined.into_iter().flatten() {
            let timeline = room
                .get("timeline")
                .and_then(|timeline| timeline.get("events"))
                .and_then(Value::as_array);
            for event in timeline.into_iter().flatten() {
                if let Some(message) = self.parse_event(room_id, event) {
                    events.push(message);
                }
            }
        }
        Ok((events, next_batch))
    }

    /// Normalize one `timeline` event into a [`MessageEvent`], or `None` when it is not a message
    /// this adapter delivers (wrong type, unsupported msgtype, our own sender, an edit).
    fn parse_event(&self, room_id: &str, event: &Value) -> Option<MessageEvent> {
        if event.get("type").and_then(Value::as_str) != Some("m.room.message") {
            return None;
        }
        let content = event.get("content")?;
        let msgtype = content
            .get("msgtype")
            .and_then(Value::as_str)
            .unwrap_or_default();
        let message_type = message_type_for(msgtype)?;

        let relates_to = content.get("m.relates_to");
        // An edit replaces an earlier event whose text already went through the pipeline.
        if relates_to
            .and_then(|relates_to| relates_to.get("rel_type"))
            .and_then(Value::as_str)
            == Some("m.replace")
        {
            return None;
        }

        let sender = event
            .get("sender")
            .and_then(Value::as_str)
            .map(str::trim)
            .filter(|sender| !sender.is_empty())?;
        if self.is_own_sender(sender) {
            return None;
        }

        let message_id = event_id_from(event);
        let text = content
            .get("body")
            .and_then(Value::as_str)
            .unwrap_or_default()
            .to_string();
        let mime_type = content
            .get("info")
            .and_then(|info| info.get("mimetype"))
            .and_then(Value::as_str)
            .map(str::trim)
            .filter(|mime| !mime.is_empty())
            .map(str::to_string);

        let mut source = SessionSource::new(matrix_platform(), room_id.to_string());
        source.chat_type = "group".to_string();
        source.user_id = Some(sender.to_string());
        source.user_name = Some(display_name_from_mxid(sender));
        source.message_id = message_id.clone();

        let mut message = MessageEvent::new(text, source);
        message.message_type = message_type;
        // hermes mirrors the author onto the event for per-message prompt builders.
        message.user_id = message.source.user_id.clone();
        message.user_name = message.source.user_name.clone();
        message.message_id = message_id;
        message.reply_to_message_id = relates_to
            .and_then(|relates_to| relates_to.get("m.in_reply_to"))
            .and_then(|in_reply_to| in_reply_to.get("event_id"))
            .and_then(Value::as_str)
            .map(str::to_string);
        // The mxc:// content itself is not downloaded (no media fetcher in this port), so the MIME
        // type travels alone and `media_urls` stays empty.
        if let Some(mime_type) = mime_type {
            message.media_types = vec![mime_type];
        }
        message.raw_message = Some(event.clone());
        Some(message)
    }

    /// Is `sender` our own account? Homeservers vary the localpart case across API surfaces, so the
    /// comparison is case-insensitive (hermes `_is_self_sender`).
    fn is_own_sender(&self, sender: &str) -> bool {
        match self.user_id.as_deref() {
            Some(own) => own.trim().eq_ignore_ascii_case(sender.trim()),
            None => false,
        }
    }
}

/// The gateway's platform handle for this adapter. The constant is a fixed non-empty name, so the
/// fallback arm is unreachable and exists only to keep this function panic-free.
fn matrix_platform() -> Platform {
    match Platform::new(platform_names::MATRIX) {
        Some(platform) => platform,
        None => Platform::local(),
    }
}

/// Percent-encode one path segment or query value: unreserved characters are kept, everything else
/// becomes `%XX` (matches Python's `quote(value, safe="")` for room ids and sync tokens).
fn percent_encode(value: &str) -> String {
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

/// `event_id` of a send/sync payload when it is a non-blank string.
fn event_id_from(value: &Value) -> Option<String> {
    value
        .get("event_id")
        .and_then(Value::as_str)
        .map(str::trim)
        .filter(|event_id| !event_id.is_empty())
        .map(str::to_string)
}

/// A Matrix user id's display fallback: the localpart (`@alice:example.org` → `alice`), else the
/// raw id (hermes `_get_display_name`'s no-profile fallback).
fn display_name_from_mxid(user_id: &str) -> String {
    let trimmed = user_id.trim();
    match trimmed.strip_prefix('@') {
        Some(rest) => match rest.split_once(':') {
            Some((localpart, _)) if !localpart.is_empty() => localpart.to_string(),
            _ => trimmed.to_string(),
        },
        None => trimmed.to_string(),
    }
}

/// Inbound `m.room.message` msgtype → gateway [`MessageType`], or `None` for msgtypes this adapter
/// does not deliver (`m.emote`, `m.location`, `m.sticker`, ... — hermes handles the same small set).
fn message_type_for(msgtype: &str) -> Option<MessageType> {
    match msgtype {
        "m.text" | "m.notice" => Some(MessageType::Text),
        "m.image" => Some(MessageType::Photo),
        "m.audio" => Some(MessageType::Audio),
        "m.video" => Some(MessageType::Video),
        "m.file" => Some(MessageType::Document),
        _ => None,
    }
}

/// The `m.room.message` content of a plain text send; `reply_to` adds the reply relation.
fn text_payload(text: &str, reply_to: Option<&str>) -> Value {
    let mut content = Map::new();
    content.insert("msgtype".to_string(), Value::String(MSGTYPE_TEXT.into()));
    content.insert("body".to_string(), Value::String(text.to_string()));
    if let Some(event_id) = reply_to {
        content.insert(
            "m.relates_to".to_string(),
            json!({ "m.in_reply_to": { "event_id": event_id } }),
        );
    }
    Value::Object(content)
}

/// The `m.reaction` content of an annotation: `m.relates_to` naming the event and the emoji key.
///
/// Byte for byte what hermes sends — `{"m.relates_to": {"rel_type": "m.annotation",
/// "event_id": event_id, "key": emoji}}` (adapter.py:2447). `key` is the emoji itself; Matrix
/// compares it verbatim, so it is passed through (only trimmed) rather than renamed the way Slack
/// names its emoji.
fn reaction_payload(event_id: &str, key: &str) -> Value {
    json!({
        "m.relates_to": {
            "rel_type": "m.annotation",
            "event_id": event_id,
            "key": key,
        },
    })
}

/// A caller string that can address something on the wire: trimmed, or `None` when blank.
fn non_blank(value: &str) -> Option<&str> {
    let trimmed = value.trim();
    if trimmed.is_empty() {
        None
    } else {
        Some(trimmed)
    }
}

/// Outbound [`MediaKind`] → the `m.room.message` msgtype it is sent as.
///
/// hermes picks the same four in its per-kind senders: `send_image` → `"m.image"`, `send_video` →
/// `"m.video"`, `send_voice` → `"m.audio"`, `send_document` → `"m.file"`.
fn msgtype_for(kind: MediaKind) -> &'static str {
    match kind {
        MediaKind::Image => "m.image",
        MediaKind::Video => "m.video",
        MediaKind::Audio => "m.audio",
        MediaKind::Document => "m.file",
    }
}

/// The upload filename for `media`: the caller's, else a per-kind default.
///
/// hermes always has a name to work with (the local file's `Path.name`, or the URL's last segment),
/// so this port — whose callers may hand over bare bytes — names the file per kind instead.
fn media_filename(media: &OutboundMedia) -> String {
    match media.filename.as_deref().map(str::trim) {
        Some(filename) if !filename.is_empty() => filename.to_string(),
        _ => match media.kind {
            MediaKind::Image => "image.png".to_string(),
            MediaKind::Video => "video.mp4".to_string(),
            MediaKind::Audio => "audio.mp3".to_string(),
            MediaKind::Document => "file".to_string(),
        },
    }
}

/// Per-kind MIME default, used when neither the caller nor the filename extension says otherwise.
fn default_mime(kind: MediaKind) -> &'static str {
    match kind {
        MediaKind::Image => "image/png",
        MediaKind::Video => "video/mp4",
        MediaKind::Audio => "audio/mpeg",
        MediaKind::Document => "application/octet-stream",
    }
}

/// MIME for a filename extension, mirroring the `mimetypes.guess_type` call hermes makes in
/// `_send_local_file` (`mimetypes.guess_type(fname)[0] or "application/octet-stream"`).
///
/// Only the extensions a gateway actually forwards are listed; anything else falls through to the
/// per-kind default in [`media_mime`].
fn mime_for_extension(filename: &str) -> Option<&'static str> {
    let extension = filename.rsplit_once('.')?.1.to_ascii_lowercase();
    match extension.as_str() {
        "png" => Some("image/png"),
        "jpg" | "jpeg" => Some("image/jpeg"),
        "gif" => Some("image/gif"),
        "webp" => Some("image/webp"),
        "bmp" => Some("image/bmp"),
        "svg" => Some("image/svg+xml"),
        "heic" => Some("image/heic"),
        "heif" => Some("image/heif"),
        "avif" => Some("image/avif"),
        "mp4" => Some("video/mp4"),
        "webm" => Some("video/webm"),
        "mov" => Some("video/quicktime"),
        "mkv" => Some("video/x-matroska"),
        "mp3" => Some("audio/mpeg"),
        "ogg" | "oga" | "opus" => Some("audio/ogg"),
        "wav" => Some("audio/wav"),
        "m4a" => Some("audio/mp4"),
        "flac" => Some("audio/flac"),
        "aac" => Some("audio/aac"),
        "amr" => Some("audio/amr"),
        "pdf" => Some("application/pdf"),
        "txt" => Some("text/plain"),
        "csv" => Some("text/csv"),
        "json" => Some("application/json"),
        "zip" => Some("application/zip"),
        _ => None,
    }
}

/// The MIME this upload declares and reports in `info.mimetype`: the caller's, else a guess from
/// the filename, else the per-kind default.
fn media_mime(media: &OutboundMedia, filename: &str) -> String {
    match media.mime.as_deref().map(str::trim) {
        Some(mime) if !mime.is_empty() => mime.to_string(),
        _ => match mime_for_extension(filename) {
            Some(guessed) => guessed.to_string(),
            None => default_mime(media.kind).to_string(),
        },
    }
}

/// The event `body`: the caption when there is one, else the filename.
///
/// hermes: `"body": caption or filename` — an empty caption falls back to the filename, which is
/// also what a client shows for an uncaptioned attachment.
fn media_body(media: &OutboundMedia, filename: &str) -> String {
    match media.caption.as_deref().map(str::trim) {
        Some(caption) if !caption.is_empty() => caption.to_string(),
        _ => filename.to_string(),
    }
}

/// The `m.room.message` content of a media send.
///
/// Shape is hermes' `_upload_and_send`: `msgtype`, `body`, `info {mimetype, size}`, then `url` with
/// the uploaded content URI (the encrypted-upload branch sets `file` instead, which this port has
/// no E2EE to produce). `size` is the original byte count. No `w`/`h` are sent: the reference fills
/// `info` with `mimetype` and `size` only and never probes dimensions on the upload path.
/// `reply_to` adds the same `m.relates_to.m.in_reply_to` relation
/// `_apply_relation_metadata` attaches.
fn media_payload(
    msgtype: &str,
    body: &str,
    content_uri: &str,
    mime: &str,
    size: usize,
    reply_to: Option<&str>,
) -> Value {
    let mut content = Map::new();
    content.insert("msgtype".to_string(), Value::String(msgtype.to_string()));
    content.insert("body".to_string(), Value::String(body.to_string()));
    content.insert("url".to_string(), Value::String(content_uri.to_string()));
    content.insert(
        "info".to_string(),
        json!({ "mimetype": mime, "size": size }),
    );
    if let Some(event_id) = reply_to {
        content.insert(
            "m.relates_to".to_string(),
            json!({ "m.in_reply_to": { "event_id": event_id } }),
        );
    }
    Value::Object(content)
}

/// `content_uri` of an upload response when it is a non-blank string.
fn content_uri_from(value: &Value) -> Option<String> {
    value
        .get("content_uri")
        .and_then(Value::as_str)
        .map(str::trim)
        .filter(|content_uri| !content_uri.is_empty())
        .map(str::to_string)
}

/// The local size gate hermes runs before uploading (`_media_too_large`).
///
/// `None` when the attachment fits; otherwise the typed refusal, with hermes' wording.
fn size_failure(size: usize) -> Option<SendResult> {
    if size <= MAX_MEDIA_BYTES {
        return None;
    }
    Some(SendResult::failed(
        format!("Media file exceeds Matrix limit ({size} > {MAX_MEDIA_BYTES} bytes)"),
        Some("too_long"),
    ))
}

/// Why URL-sourced media cannot be sent, and what to do instead.
///
/// hermes never forwards a URL: `send_image` downloads an http(s) URL through
/// `_download_external_media_with_cap` (SSRF check, size cap, `image/*` only) and uploads the bytes,
/// while the inbound path refuses any `url` that is not `mxc://`. This port has no fetcher, so both
/// cases are refused with the reason rather than a silent no-op or a fabricated upload.
fn url_refusal(url: &str) -> SendResult {
    let scheme = url.split_once(':').map(|(scheme, _)| scheme).unwrap_or("");
    let reason = if scheme.eq_ignore_ascii_case("mxc") {
        "an mxc:// URL is a content URI this port cannot resolve into bytes, and hermes uploads \
         downloaded bytes rather than forwarding a URI"
    } else {
        "hermes downloads external media itself (SSRF-guarded, size-capped) before uploading; \
         this port has no URL fetcher, and a non-mxc:// URL is refused on the inbound path too"
    };
    SendResult::failed(
        format!(
            "matrix media from a URL is not supported: {reason}. Pass MediaSource::Bytes instead."
        ),
        Some("bad_format"),
    )
}

/// The Matrix error fields a failure carries: `errcode` (`M_...`), the human `error` text, and the
/// optional `retry_after_ms` a rate-limit body includes.
#[derive(Debug, Default)]
struct MatrixError {
    errcode: Option<String>,
    detail: Option<String>,
    retry_after_ms: Option<f64>,
}

impl MatrixError {
    /// Decode `{"errcode": "...", "error": "...", "retry_after_ms": N}`; absent fields stay `None`.
    fn from_body(body: &str) -> Self {
        let Ok(decoded) = serde_json::from_str::<Value>(body) else {
            return Self::default();
        };
        Self {
            errcode: decoded
                .get("errcode")
                .and_then(Value::as_str)
                .map(str::trim)
                .filter(|code| !code.is_empty())
                .map(str::to_string),
            detail: decoded
                .get("error")
                .and_then(Value::as_str)
                .map(str::trim)
                .filter(|detail| !detail.is_empty())
                .map(str::to_string),
            retry_after_ms: decoded.get("retry_after_ms").and_then(Value::as_f64),
        }
    }

    /// Server-requested delay in seconds: `retry_after_ms` converted, else the `Retry-After` header
    /// the transport already parsed.
    fn retry_after_secs(&self, header: Option<f64>) -> Option<f64> {
        match self.retry_after_ms {
            Some(millis) => Some(millis / 1000.0),
            None => header,
        }
    }
}

/// Map a non-2xx Matrix response onto a classified [`SendResult`].
///
/// The `errcode` drives the taxonomy because Matrix reuses statuses across causes:
/// `M_LIMIT_EXCEEDED` → `rate_limited` (retryable with the body's delay), `M_FORBIDDEN` /
/// `M_UNKNOWN_TOKEN` / `M_MISSING_TOKEN` → `forbidden`, `M_NOT_FOUND` / `M_UNKNOWN` (and
/// `M_UNRECOGNIZED`, e.g. a rejected verb) → `not_found`. Without a usable errcode the status
/// decides: 429 → `rate_limited`, 401/403 → `forbidden`, 404 → `not_found`, other 4xx →
/// `bad_format`, 5xx → `transient` (retryable), anything else → `unknown`.
fn classify_failure(response: &HttpResponse) -> SendResult {
    let error = MatrixError::from_body(&response.body);
    let message = match (&error.errcode, &error.detail) {
        (Some(errcode), Some(detail)) => {
            format!(
                "matrix api returned {}: {errcode}: {detail}",
                response.status
            )
        }
        (Some(errcode), None) => format!("matrix api returned {}: {errcode}", response.status),
        (None, _) => format!(
            "matrix api returned {}: {}",
            response.status,
            body_excerpt(&response.body)
        ),
    };

    match error.errcode.as_deref() {
        Some("M_LIMIT_EXCEEDED") => SendResult::failed(message, Some("rate_limited"))
            .retryable(error.retry_after_secs(response.retry_after)),
        Some("M_FORBIDDEN" | "M_UNKNOWN_TOKEN" | "M_MISSING_TOKEN") => {
            SendResult::failed(message, Some("forbidden"))
        }
        Some("M_NOT_FOUND" | "M_UNKNOWN" | "M_UNRECOGNIZED") => {
            SendResult::failed(message, Some("not_found"))
        }
        _ => match response.status {
            429 => SendResult::failed(message, Some("rate_limited"))
                .retryable(error.retry_after_secs(response.retry_after)),
            401 | 403 => SendResult::failed(message, Some("forbidden")),
            404 => SendResult::failed(message, Some("not_found")),
            500..=599 => SendResult::failed(message, Some("transient")).retryable(None),
            _ => SendResult::failed(message, Some("bad_format")),
        },
    }
}

/// Classify a transport-level failure: the request never produced a response, so the send is
/// retryable (`transient`).
fn transport_failure(err: &TransportError) -> SendResult {
    SendResult::failed(format!("matrix transport error: {err}"), Some("transient")).retryable(None)
}

/// [`classify_failure`] plus the one media-only case: an attachment over the limit.
///
/// Matrix answers an oversized upload (and an oversized event) with `413` / `M_TOO_LARGE`, which
/// plain text sends never cause — the gateway taxonomy calls that `too_long` ("the payload will
/// never fit"), not `bad_format`. Not retryable: the same bytes fail the same way. Everything else
/// (429 with `retry_after_ms`, `M_UNKNOWN_TOKEN`/403, 404, 5xx, errcode precedence) is delegated,
/// so media and text classify identically.
fn classify_media_failure(response: &HttpResponse) -> SendResult {
    let error = MatrixError::from_body(&response.body);
    if error.errcode.as_deref() == Some("M_TOO_LARGE") || response.status == 413 {
        let detail = error
            .detail
            .or(error.errcode)
            .unwrap_or_else(|| body_excerpt(&response.body));
        return SendResult::failed(
            format!("matrix media rejected as too large: {detail}"),
            Some("too_long"),
        );
    }
    classify_failure(response)
}

#[async_trait]
impl PlatformAdapter for MatrixAdapter {
    /// Probe `GET /account/whoami`; true when the homeserver answers with a user id.
    ///
    /// The resolved id is remembered (own-message filtering and the typing endpoint need it) and
    /// dropped again on failure, so a stale identity never outlives the token that proved it.
    async fn connect(&mut self, _is_reconnect: bool) -> bool {
        let request = self.authorized(HttpRequest::get(self.endpoint("/account/whoami")));
        let resolved = match self.transport.execute(request).await {
            Ok(response) if response.ok() => response
                .json()
                .ok()
                .as_ref()
                .and_then(|body| body.get("user_id"))
                .and_then(Value::as_str)
                .map(str::trim)
                .filter(|user_id| !user_id.is_empty())
                .map(str::to_string),
            _ => None,
        };
        self.user_id = resolved;
        self.user_id.is_some()
    }

    /// Nothing to tear down: the adapter holds no connection, only a transport handle. The resolved
    /// identity is forgotten so a later `connect` must prove the token again.
    async fn disconnect(&mut self) {
        self.user_id = None;
    }

    /// Send `content` into `room_id` as one or more `m.room.message` events.
    ///
    /// Content is chunked with [`truncate_message`] to [`MAX_MESSAGE_LENGTH`] and the chunks go out
    /// sequentially; each gets a fresh transaction id. `reply_to` anchors the first chunk only
    /// (hermes' default `reply_to_mode = "first"`). A failure after some chunks landed keeps them
    /// and reports `partial_overflow`, so the delivery ledger resends only the tail.
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
        let chunks = truncate_message(content, self.max_message_length(), codepoint_len);
        let total = chunks.len();
        if total == 0 {
            return SendResult::failed("Refusing to send empty message", Some("bad_format"));
        }

        let mut ids: Vec<String> = Vec::new();
        for (index, chunk) in chunks.iter().enumerate() {
            let anchor = if index == 0 { reply_to } else { None };
            let payload = text_payload(chunk, anchor);
            let path = self.send_event_path(chat_id, &self.txn_id());
            match self.put_event(&path, &payload).await {
                Ok(event_id) => ids.push(event_id),
                Err(failure) => {
                    if let Some(last) = ids.last().cloned() {
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
            None => SendResult::failed("matrix send produced no events", Some("bad_format")),
        }
    }

    /// Whether [`send_media`](Self::send_media) is implemented here: it is (upload + send), so the
    /// router may hand attachments straight to this adapter instead of the text fallback.
    fn supports_media(&self) -> bool {
        true
    }

    /// Reactions are implemented: `m.reaction` annotations, removed by redacting them.
    fn supports_reactions(&self) -> bool {
        true
    }

    /// React to `message_id` in `chat_id` with one `m.reaction` annotation event.
    ///
    /// `PUT /rooms/{room}/send/m.reaction/{txn}` carrying `m.relates_to: {rel_type: "m.annotation",
    /// event_id, key}` — hermes' `_send_reaction` (adapter.py:2443-2454), which reacts with a glyph
    /// (`👀`, `✅`, `❌`) and keeps the returned reaction event id so it can withdraw it later. This
    /// port stores that id under `(room, message, key)` for [`remove_reaction`](Self::remove_reaction)
    /// and reports `true` only when the homeserver accepted the event *and* named it; a failure
    /// leaves the cache untouched, so a retry cannot redact an event that was never created.
    ///
    /// Blank room / event / emoji input is refused before any request.
    async fn add_reaction(&self, chat_id: &str, message_id: &str, emoji: &str) -> bool {
        let (Some(room_id), Some(event_id), Some(key)) =
            (non_blank(chat_id), non_blank(message_id), non_blank(emoji))
        else {
            return false;
        };
        match self.send_reaction(room_id, event_id, key).await {
            Ok(reaction_event_id) => {
                self.remember_reaction(room_id, event_id, key, &reaction_event_id);
                true
            }
            Err(_) => false,
        }
    }

    /// Withdraw this bot's reaction by redacting the reaction event that carried it.
    ///
    /// Matrix has no "unreact" call: hermes removes a reaction by redacting the event
    /// `_send_reaction` created (`_redact_reaction` → `redact_message`, adapter.py:2456-2457 /
    /// 2708-2711). The same holds here, which is why the reaction event id has to be remembered:
    /// with no id recorded for `(room, message, key)` the answer is `false` and **no request is
    /// made** — redacting an event the bot is not sure about could delete a message instead of an
    /// annotation. A successful redaction forgets the entry; a failed one keeps it, because the
    /// annotation is still on the message and a retry should target the same event.
    async fn remove_reaction(&self, chat_id: &str, message_id: &str, emoji: &str) -> bool {
        let (Some(room_id), Some(event_id), Some(key)) =
            (non_blank(chat_id), non_blank(message_id), non_blank(emoji))
        else {
            return false;
        };
        let Some(reaction_event_id) = self.stored_reaction(room_id, event_id, key) else {
            return false;
        };
        match self.redact_reaction(room_id, &reaction_event_id).await {
            Ok(_) => {
                self.forget_reaction(room_id, event_id, key);
                true
            }
            Err(_) => false,
        }
    }

    /// Upload one attachment, then send the `m.room.message` that references it.
    ///
    /// Mirrors hermes' `_upload_and_send`: `POST /_matrix/media/v3/upload?filename=…` (the MIME as
    /// `Content-Type`, the bytes as the body) answers with a `content_uri`, which the follow-up
    /// `PUT /rooms/{room}/send/m.room.message/{txn}` puts in `url` next to
    /// `{msgtype, body, info{mimetype, size}}`. `body` is the caption when there is one, else the
    /// filename, and `reply_to` adds the `m.in_reply_to` relation.
    ///
    /// Failures are never reported as deliveries: an upload failure returns that failure with no
    /// send attempted, a send failure returns that failure (the media is uploaded but not delivered),
    /// and local refusals (empty bytes, over [`MAX_MEDIA_BYTES`], `MediaSource::Url`) make no request
    /// at all.
    async fn send_media(
        &self,
        chat_id: &str,
        media: &OutboundMedia,
        reply_to: Option<&str>,
    ) -> SendResult {
        let data = match &media.source {
            MediaSource::Bytes(bytes) => bytes,
            MediaSource::Url(url) => return url_refusal(url),
        };
        if data.is_empty() {
            // Nothing to upload: a 0-byte file is a caller bug, not a platform problem.
            return SendResult::failed("Refusing to upload empty media", Some("bad_format"));
        }
        if let Some(failure) = size_failure(data.len()) {
            return failure;
        }
        self.send_media_event(chat_id, media, data, reply_to).await
    }

    /// `GET /rooms/{room}/state/m.room.name`, falling back to `m.room.canonical_alias`.
    ///
    /// Matrix rooms are groups unless a direct flag is known (the `m.direct` account data is not
    /// fetched here), so `chat_type` is always `"group"`. Any failure degrades to
    /// `{name: chat_id, chat_type: "group"}` so routing still works.
    async fn get_chat_info(&self, chat_id: &str) -> ChatInfo {
        let name = match self.room_state_string(chat_id, "m.room.name", "name").await {
            Some(name) => Some(name),
            None => {
                self.room_state_string(chat_id, "m.room.canonical_alias", "alias")
                    .await
            }
        };
        ChatInfo {
            name: name.unwrap_or_else(|| chat_id.to_string()),
            chat_type: "group".to_string(),
        }
    }

    /// Replace an event's content with `m.relates_to.rel_type = "m.replace"` (Matrix edits are new
    /// events pointing at the old one).
    ///
    /// The top-level `body` carries the spec's `* `-prefixed fallback for clients that do not
    /// render edits, while `m.new_content` holds what edit-aware clients display. The result's
    /// `message_id` is the **edit event's** id, not the replaced event's.
    async fn edit_message(
        &self,
        chat_id: &str,
        message_id: &str,
        content: &str,
        _finalize: bool,
    ) -> SendResult {
        if content.trim().is_empty() {
            return SendResult::failed("Refusing to send empty edit", Some("bad_format"));
        }
        let payload = json!({
            "msgtype": MSGTYPE_TEXT,
            "body": format!("* {content}"),
            "m.new_content": text_payload(content, None),
            "m.relates_to": { "rel_type": "m.replace", "event_id": message_id },
        });
        let path = self.send_event_path(chat_id, &self.txn_id());
        match self.put_event(&path, &payload).await {
            Ok(event_id) => SendResult::ok(event_id),
            Err(failure) => *failure,
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
        MAX_MESSAGE_LENGTH
    }

    /// Best-effort `PUT /rooms/{room}/typing/{user_id}`; Matrix shows the indicator for 30 s.
    ///
    /// Needs the user id `connect` resolves — without it this is a no-op, because a typing event
    /// addressed to the wrong user id is a 403. Failures (429 included) are dropped: the indicator
    /// is cosmetic.
    async fn send_typing(&self, chat_id: &str, _metadata: Option<Value>) {
        let Some(user_id) = self.user_id.as_deref() else {
            return;
        };
        let path = format!(
            "/rooms/{}/typing/{}",
            percent_encode(chat_id),
            percent_encode(user_id)
        );
        let payload = json!({ "typing": true, "timeout": TYPING_TIMEOUT_MS });
        let _ = self.transport.execute(self.put_json(&path, &payload)).await;
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

    /// Records every request and replays queued responses (an `{"event_id":"fallback"}` 200 once
    /// drained).
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

        /// Queue responses and transport errors in call order.
        fn scripted(script: Vec<Result<HttpResponse, TransportError>>) -> Arc<Self> {
            let mock = MockTransport::default();
            lock(&mock.responses).extend(script);
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
                None => Ok(json_response(200, json!({"event_id": "$fallback"}))),
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

    const HOMESERVER: &str = "https://matrix.example.org";

    fn adapter(mock: &Arc<MockTransport>) -> MatrixAdapter {
        MatrixAdapter::new(mock.clone(), HOMESERVER, "test-token")
    }

    /// A `whoami` response, queued when a test needs `connect` to succeed first.
    fn whoami_response() -> HttpResponse {
        json_response(200, json!({"user_id": "@bot:example.org"}))
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

    fn event_ids(count: usize) -> Vec<HttpResponse> {
        (0..count)
            .map(|idx| json_response(200, json!({ "event_id": format!("$evt{idx}") })))
            .collect()
    }

    /// The `/send/m.room.message/{txn}` transaction id of a send request.
    fn txn_of(request: &HttpRequest) -> String {
        request
            .url
            .rsplit('/')
            .next()
            .unwrap_or_default()
            .to_string()
    }

    fn sync_body(events: Value, next_batch: &str) -> Value {
        json!({
            "next_batch": next_batch,
            "rooms": { "join": { "!room:example.org": { "timeline": { "events": events } } } },
        })
    }

    #[tokio::test]
    async fn connect_probes_whoami_with_the_access_token() {
        let mock = MockTransport::with(vec![json_response(
            200,
            json!({"user_id": "@bot:example.org", "device_id": "DEV"}),
        )]);
        let mut matrix = adapter(&mock);
        assert!(matrix.connect(false).await);

        let requests = mock.requests();
        assert_eq!(requests.len(), 1);
        assert_eq!(requests[0].method, HttpMethod::Get);
        assert_eq!(
            requests[0].url,
            "https://matrix.example.org/_matrix/client/v3/account/whoami"
        );
        assert_eq!(
            header(&requests[0], "Authorization"),
            Some("Bearer test-token")
        );
        assert_eq!(matrix.user_id(), Some("@bot:example.org"));
        assert_eq!(matrix.homeserver(), HOMESERVER);
    }

    #[tokio::test]
    async fn connect_fails_without_a_user_id() {
        let mock = MockTransport::with(vec![
            json_response(
                401,
                json!({"errcode": "M_UNKNOWN_TOKEN", "error": "Invalid access token"}),
            ),
            json_response(200, json!({"user_id": "   "})),
            json_response(200, Value::Null),
        ]);
        let mut matrix = adapter(&mock);
        assert!(!matrix.connect(false).await);
        assert!(!matrix.connect(true).await);
        assert!(!matrix.connect(true).await);
        assert_eq!(matrix.user_id(), None);

        let offline = MockTransport::failing(TransportError::Request("boom".to_string()));
        let mut unreachable = adapter(&offline);
        assert!(!unreachable.connect(false).await);
    }

    #[tokio::test]
    async fn send_writes_a_text_event_with_txn_and_bearer() {
        let mock = MockTransport::with(vec![json_response(200, json!({"event_id": "$e1"}))]);
        let result = adapter(&mock)
            .send("!room:example.org", "hello", None, None)
            .await;
        assert!(result.success, "{:?}", result.error);
        assert_eq!(result.message_id.as_deref(), Some("$e1"));
        assert_eq!(result.continuation_message_ids, vec!["$e1"]);

        let requests = mock.requests();
        assert_eq!(requests.len(), 1);
        assert_eq!(requests[0].method, HttpMethod::Put);
        assert!(
            requests[0].url.starts_with(
                "https://matrix.example.org/_matrix/client/v3/rooms/%21room%3Aexample.org/send/m.room.message/"
            ),
            "url was {}",
            requests[0].url
        );
        assert!(txn_of(&requests[0]).starts_with(&format!("vikram-{}", std::process::id())));
        assert_eq!(
            header(&requests[0], "Authorization"),
            Some("Bearer test-token")
        );
        assert_eq!(
            header(&requests[0], "Content-Type"),
            Some("application/json")
        );
        let body = body_json(&requests[0]);
        assert_eq!(body["msgtype"], "m.text");
        assert_eq!(body["body"], "hello");
        assert!(body.get("m.relates_to").is_none());
    }

    #[tokio::test]
    async fn transaction_ids_are_unique_per_call() {
        let mock = MockTransport::with(event_ids(3));
        let matrix = adapter(&mock);
        assert!(
            matrix
                .send("!r:example.org", "one", None, None)
                .await
                .success
        );
        assert!(
            matrix
                .send("!r:example.org", "two", None, None)
                .await
                .success
        );
        assert!(
            matrix
                .send("!r:example.org", "three", None, None)
                .await
                .success
        );

        let ids: Vec<String> = mock.requests().iter().map(txn_of).collect();
        assert_eq!(ids.len(), 3);
        assert_ne!(ids[0], ids[1]);
        assert_ne!(ids[1], ids[2]);
        assert_ne!(ids[0], ids[2]);

        // A second adapter in the same process gets a different prefix.
        let other = adapter(&mock);
        assert!(
            other
                .send("!r:example.org", "four", None, None)
                .await
                .success
        );
        assert_ne!(txn_of(&mock.requests()[3]), ids[0]);
    }

    #[tokio::test]
    async fn send_attaches_the_reply_relation() {
        let mock = MockTransport::with(vec![json_response(200, json!({"event_id": "$e2"}))]);
        assert!(
            adapter(&mock)
                .send("!room:example.org", "answering", Some("$orig"), None)
                .await
                .success
        );
        let body = body_json(&mock.requests()[0]);
        assert_eq!(body["m.relates_to"]["m.in_reply_to"]["event_id"], "$orig");
        assert_eq!(body["msgtype"], "m.text");
    }

    #[tokio::test]
    async fn reply_relation_anchors_only_the_first_chunk() {
        let mock = MockTransport::with(event_ids(4));
        let content = "word ".repeat(4_000);
        assert!(
            adapter(&mock)
                .send("!room:example.org", &content, Some("$orig"), None)
                .await
                .success
        );
        let requests = mock.requests();
        assert!(requests.len() > 1, "expected several chunks");
        for (index, request) in requests.iter().enumerate() {
            let body = body_json(request);
            match body.get("m.relates_to") {
                Some(relates_to) => {
                    assert_eq!(index, 0, "only the first chunk anchors the reply");
                    assert_eq!(relates_to["m.in_reply_to"]["event_id"], "$orig");
                }
                None => assert_ne!(index, 0),
            }
        }
    }

    #[tokio::test]
    async fn send_chunks_long_content_within_the_budget() {
        let mock = MockTransport::with(event_ids(8));
        let content = "word ".repeat(4_000); // 20 000 characters, three chunks
        let result = adapter(&mock)
            .send("!room:example.org", &content, None, None)
            .await;
        assert!(result.success, "{:?}", result.error);

        let requests = mock.requests();
        assert_eq!(requests.len(), 3);
        for request in &requests {
            let body = body_json(request);
            let chunk = body["body"].as_str().unwrap_or_default();
            assert!(
                codepoint_len(chunk) <= MAX_MESSAGE_LENGTH,
                "chunk measures {}",
                codepoint_len(chunk)
            );
            assert!(!chunk.trim().is_empty());
        }
        let last = requests.len() - 1;
        assert_eq!(
            result.message_id.as_deref(),
            Some(format!("$evt{last}").as_str())
        );
        assert_eq!(result.continuation_message_ids.len(), requests.len());
    }

    #[tokio::test]
    async fn rate_limit_is_retryable_with_the_converted_delay() {
        let mock = MockTransport::with(vec![json_response(
            429,
            json!({
                "errcode": "M_LIMIT_EXCEEDED",
                "error": "Too many requests",
                "retry_after_ms": 2500,
            }),
        )]);
        let result = adapter(&mock)
            .send("!room:example.org", "hi", None, None)
            .await;
        assert!(!result.success);
        assert!(result.retryable);
        assert_eq!(result.error_kind.as_deref(), Some("rate_limited"));
        assert_eq!(result.retry_after, Some(2.5));

        // Without a body delay the Retry-After header is already in seconds.
        let header_only = MockTransport::with(vec![HttpResponse {
            status: 429,
            body: json!({"errcode": "M_LIMIT_EXCEEDED"}).to_string(),
            retry_after: Some(1.5),
        }]);
        let result = adapter(&header_only)
            .send("!room:example.org", "hi", None, None)
            .await;
        assert!(result.retryable);
        assert_eq!(result.retry_after, Some(1.5));
    }

    #[tokio::test]
    async fn auth_errcodes_map_to_forbidden() {
        for (status, errcode) in [
            (403u16, "M_FORBIDDEN"),
            (401, "M_UNKNOWN_TOKEN"),
            (401, "M_MISSING_TOKEN"),
        ] {
            let mock = MockTransport::with(vec![json_response(
                status,
                json!({"errcode": errcode, "error": "nope"}),
            )]);
            let result = adapter(&mock)
                .send("!room:example.org", "hi", None, None)
                .await;
            assert!(!result.success, "{errcode}");
            assert_eq!(result.error_kind.as_deref(), Some("forbidden"), "{errcode}");
            assert!(!result.retryable, "{errcode}");
            assert!(
                result
                    .error
                    .as_deref()
                    .unwrap_or_default()
                    .contains(errcode),
                "the errcode should surface in the message"
            );
        }
    }

    #[tokio::test]
    async fn errcodes_and_statuses_split_not_found_bad_format_and_transient() {
        for errcode in ["M_NOT_FOUND", "M_UNKNOWN"] {
            let mock = MockTransport::with(vec![json_response(
                400,
                json!({"errcode": errcode, "error": "gone"}),
            )]);
            let result = adapter(&mock)
                .send("!room:example.org", "hi", None, None)
                .await;
            assert_eq!(result.error_kind.as_deref(), Some("not_found"), "{errcode}");
        }

        // A plain 400 without an errcode is a bad payload.
        let invalid = MockTransport::with(vec![json_response(
            400,
            json!({"error": "Malformed request"}),
        )]);
        let result = adapter(&invalid)
            .send("!room:example.org", "hi", None, None)
            .await;
        assert_eq!(result.error_kind.as_deref(), Some("bad_format"));
        assert!(!result.retryable);

        let server = MockTransport::with(vec![json_response(503, json!({"error": "down"}))]);
        let result = adapter(&server)
            .send("!room:example.org", "hi", None, None)
            .await;
        assert_eq!(result.error_kind.as_deref(), Some("transient"));
        assert!(result.retryable);

        let offline = MockTransport::failing(TransportError::Request("connection reset".into()));
        let result = adapter(&offline)
            .send("!room:example.org", "hi", None, None)
            .await;
        assert_eq!(result.error_kind.as_deref(), Some("transient"));
        assert!(result.retryable);
    }

    #[tokio::test]
    async fn blank_content_is_rejected_without_a_request() {
        let mock = MockTransport::with(Vec::new());
        let matrix = adapter(&mock);
        for content in ["", "   ", "\n\t \r\n"] {
            let result = matrix.send("!room:example.org", content, None, None).await;
            assert!(!result.success, "content {content:?}");
            assert_eq!(result.error_kind.as_deref(), Some("bad_format"));
        }
        assert_eq!(mock.request_count(), 0);
    }

    #[tokio::test]
    async fn partial_delivery_keeps_the_chunks_that_landed() {
        let mut responses = event_ids(2);
        responses.push(json_response(500, json!({"error": "boom"})));
        let mock = MockTransport::with(responses);
        let result = adapter(&mock)
            .send("!room:example.org", &"y".repeat(20_000), None, None)
            .await;
        assert!(result.success);
        assert_eq!(result.message_id.as_deref(), Some("$evt1"));
        assert_eq!(result.continuation_message_ids, vec!["$evt0", "$evt1"]);
        let (delivered, total) = result.partial_overflow().unwrap_or_default();
        assert_eq!(delivered, 2);
        assert_eq!(total, 3);
    }

    #[tokio::test]
    async fn edit_message_sends_a_replace_relation() {
        let mock = MockTransport::with(vec![json_response(200, json!({"event_id": "$edit1"}))]);
        let result = adapter(&mock)
            .edit_message("!room:example.org", "$orig", "new text", false)
            .await;
        assert!(result.success, "{:?}", result.error);
        assert_eq!(result.message_id.as_deref(), Some("$edit1"));

        let requests = mock.requests();
        assert_eq!(requests.len(), 1);
        assert!(
            requests[0].url.starts_with(
                "https://matrix.example.org/_matrix/client/v3/rooms/%21room%3Aexample.org/send/m.room.message/"
            ),
            "url was {}",
            requests[0].url
        );
        let body = body_json(&requests[0]);
        assert_eq!(body["m.relates_to"]["rel_type"], "m.replace");
        assert_eq!(body["m.relates_to"]["event_id"], "$orig");
        assert_eq!(body["m.new_content"]["msgtype"], "m.text");
        assert_eq!(body["m.new_content"]["body"], "new text");
        assert_eq!(body["body"], "* new text");
        assert_eq!(body["msgtype"], "m.text");
    }

    #[tokio::test]
    async fn chat_info_reads_the_room_name() {
        let mock = MockTransport::with(vec![json_response(200, json!({"name": "Release room"}))]);
        let info = adapter(&mock).get_chat_info("!room:example.org").await;
        assert_eq!(
            info,
            ChatInfo {
                name: "Release room".to_string(),
                chat_type: "group".to_string(),
            }
        );

        let requests = mock.requests();
        assert_eq!(requests.len(), 1);
        assert_eq!(requests[0].method, HttpMethod::Get);
        assert_eq!(
            requests[0].url,
            "https://matrix.example.org/_matrix/client/v3/rooms/%21room%3Aexample.org/state/m.room.name"
        );
        assert_eq!(
            header(&requests[0], "Authorization"),
            Some("Bearer test-token")
        );
    }

    #[tokio::test]
    async fn chat_info_falls_back_to_the_canonical_alias_then_the_room_id() {
        let alias = MockTransport::with(vec![
            json_response(404, json!({"errcode": "M_NOT_FOUND", "error": "no name"})),
            json_response(200, json!({"alias": "#release:example.org"})),
        ]);
        assert_eq!(
            adapter(&alias)
                .get_chat_info("!room:example.org")
                .await
                .name,
            "#release:example.org"
        );
        let requests = alias.requests();
        assert_eq!(requests.len(), 2);
        assert!(
            requests[1].url.ends_with("/state/m.room.canonical_alias"),
            "url was {}",
            requests[1].url
        );

        let missing = MockTransport::with(vec![
            json_response(404, json!({"errcode": "M_NOT_FOUND"})),
            json_response(200, json!({"alias": "  "})),
        ]);
        assert_eq!(
            adapter(&missing).get_chat_info("!room:example.org").await,
            ChatInfo {
                name: "!room:example.org".to_string(),
                chat_type: "group".to_string(),
            }
        );

        let broken = MockTransport::with(vec![HttpResponse {
            status: 200,
            body: "not json".to_string(),
            retry_after: None,
        }]);
        assert_eq!(
            adapter(&broken)
                .get_chat_info("!room:example.org")
                .await
                .chat_type,
            "group"
        );
    }

    #[tokio::test]
    async fn poll_once_parses_text_and_media_events_and_skips_own_messages() {
        let events = json!([
            {
                "type": "m.room.message",
                "event_id": "$text1",
                "sender": "@alice:example.org",
                "origin_server_ts": 1_700_000_000_000i64,
                "content": {
                    "msgtype": "m.text",
                    "body": "hello bot",
                    "m.relates_to": { "m.in_reply_to": { "event_id": "$earlier" } }
                }
            },
            {
                "type": "m.room.message",
                "event_id": "$img1",
                "sender": "@bob:example.org",
                "content": {
                    "msgtype": "m.image",
                    "body": "cat.png",
                    "url": "mxc://example.org/abc",
                    "info": { "mimetype": "image/png", "size": 1024 }
                }
            },
            {
                "type": "m.room.message",
                "event_id": "$own1",
                "sender": "@bot:example.org",
                "content": { "msgtype": "m.text", "body": "my own answer" }
            },
            {
                "type": "m.room.message",
                "event_id": "$edit1",
                "sender": "@alice:example.org",
                "content": {
                    "msgtype": "m.text",
                    "body": "* edited",
                    "m.relates_to": { "rel_type": "m.replace", "event_id": "$text1" }
                }
            },
            {
                "type": "m.room.message",
                "event_id": "$emote1",
                "sender": "@alice:example.org",
                "content": { "msgtype": "m.emote", "body": "waves" }
            },
            {
                "type": "m.room.member",
                "event_id": "$member1",
                "sender": "@alice:example.org",
                "content": { "membership": "join" }
            }
        ]);
        let mock = MockTransport::with(vec![
            whoami_response(),
            json_response(200, sync_body(events, "s1_2_3")),
        ]);
        let mut matrix = adapter(&mock);
        assert!(matrix.connect(false).await);
        let (messages, next_batch) = matrix.poll_once(None, 30_000).await.expect("sync");
        assert_eq!(next_batch.as_deref(), Some("s1_2_3"));
        assert_eq!(
            messages.len(),
            2,
            "own events, edits, m.emote and state are skipped"
        );

        let text = &messages[0];
        assert_eq!(text.text, "hello bot");
        assert_eq!(text.message_type, MessageType::Text);
        assert_eq!(text.message_id.as_deref(), Some("$text1"));
        assert_eq!(text.source.chat_id, "!room:example.org");
        assert_eq!(text.source.chat_type, "group");
        assert_eq!(text.source.platform.as_str(), "matrix");
        assert_eq!(text.source.user_id.as_deref(), Some("@alice:example.org"));
        assert_eq!(text.user_id.as_deref(), Some("@alice:example.org"));
        assert_eq!(text.user_name.as_deref(), Some("alice"));
        assert_eq!(text.reply_to_message_id.as_deref(), Some("$earlier"));
        assert!(text.raw_message.is_some());

        let image = &messages[1];
        assert_eq!(image.message_type, MessageType::Photo);
        assert_eq!(image.text, "cat.png");
        assert_eq!(image.media_types, vec!["image/png".to_string()]);
        assert!(image.media_urls.is_empty());
        assert_eq!(image.reply_to_message_id, None);
    }

    #[tokio::test]
    async fn poll_once_sends_the_since_token_and_timeout() {
        let mock = MockTransport::with(vec![json_response(200, sync_body(json!([]), "s9"))]);
        let matrix = adapter(&mock);
        let (messages, next_batch) = matrix.poll_once(Some("s1+2/3"), 5_000).await.expect("sync");
        assert!(messages.is_empty());
        assert_eq!(next_batch.as_deref(), Some("s9"));

        let requests = mock.requests();
        assert_eq!(requests.len(), 1);
        assert_eq!(requests[0].method, HttpMethod::Get);
        assert_eq!(
            requests[0].url,
            "https://matrix.example.org/_matrix/client/v3/sync?timeout=5000&since=s1%2B2%2F3"
        );
        assert_eq!(
            header(&requests[0], "Authorization"),
            Some("Bearer test-token")
        );
    }

    #[tokio::test]
    async fn poll_once_reports_transport_and_payload_failures() {
        let failing = MockTransport::failing(TransportError::Request("down".into()));
        let error = adapter(&failing)
            .poll_once(None, 0)
            .await
            .expect_err("error");
        assert!(error.contains("transport"), "{error}");

        let rejected = MockTransport::with(vec![json_response(
            403,
            json!({"errcode": "M_FORBIDDEN", "error": "no sync for you"}),
        )]);
        let error = adapter(&rejected)
            .poll_once(None, 0)
            .await
            .expect_err("error");
        assert!(error.contains("403"), "{error}");

        let broken = MockTransport::with(vec![HttpResponse {
            status: 200,
            body: "not json".to_string(),
            retry_after: None,
        }]);
        let error = adapter(&broken)
            .poll_once(None, 0)
            .await
            .expect_err("error");
        assert!(error.contains("invalid json"), "{error}");
    }

    #[tokio::test]
    async fn typing_targets_the_user_typing_endpoint() {
        let mock = MockTransport::with(vec![
            json_response(200, json!({"user_id": "@bot:example.org"})),
            json_response(200, json!({})),
        ]);
        let mut matrix = adapter(&mock);
        assert!(matrix.connect(false).await);
        matrix.send_typing("!room:example.org", None).await;

        let requests = mock.requests();
        assert_eq!(requests.len(), 2);
        assert_eq!(
            requests[1].url,
            "https://matrix.example.org/_matrix/client/v3/rooms/%21room%3Aexample.org/typing/%40bot%3Aexample.org"
        );
        assert_eq!(
            header(&requests[1], "Authorization"),
            Some("Bearer test-token")
        );
        assert_eq!(requests[1].method, HttpMethod::Put);
        let body = body_json(&requests[1]);
        assert_eq!(body["typing"], true);
        assert_eq!(body["timeout"], TYPING_TIMEOUT_MS);
    }

    #[tokio::test]
    async fn typing_without_a_resolved_user_id_is_a_no_op() {
        let mock = MockTransport::with(Vec::new());
        adapter(&mock).send_typing("!room:example.org", None).await;
        assert_eq!(mock.request_count(), 0);

        // A failing typing call is swallowed: the indicator is cosmetic.
        let failing = MockTransport::scripted(vec![
            Ok(whoami_response()),
            Err(TransportError::Request("down".to_string())),
        ]);
        let mut matrix = adapter(&failing);
        assert!(matrix.connect(false).await);
        matrix.send_typing("!room:example.org", None).await;
        assert_eq!(failing.request_count(), 2);
    }

    #[tokio::test]
    async fn api_base_override_and_capability_flags() {
        let mock = MockTransport::with(vec![json_response(200, json!({"event_id": "$e"}))]);
        let mut matrix = MatrixAdapter::new(mock.clone(), "https://ignored.example.org", "t")
            .with_api_base("http://localhost:8008/_matrix/client/v3/");
        assert!(
            matrix
                .send("!room:example.org", "hi", None, None)
                .await
                .success
        );
        assert!(
            mock.requests()[0]
                .url
                .starts_with("http://localhost:8008/_matrix/client/v3/rooms/"),
            "url was {}",
            mock.requests()[0].url
        );

        assert!(matrix.splits_long_messages());
        assert!(matrix.supports_code_blocks());
        assert_eq!(matrix.max_message_length(), MAX_MESSAGE_LENGTH);
        assert_eq!(matrix.message_length("héllo"), 5);
        assert_eq!(matrix.typed_command_prefix(), "/");
        matrix.disconnect().await;
        assert_eq!(matrix.user_id(), None);
    }

    // ----- reactions: m.reaction annotations and their redaction -----

    /// The reaction event id the successful-annotation fixtures return.
    const REACTION_EVENT: &str = "$react1";

    /// Room path prefix every reaction call of these tests hangs off.
    const ROOM_PATH: &str =
        "https://matrix.example.org/_matrix/client/v3/rooms/%21room%3Aexample.org";

    /// A successful `m.reaction` send: the homeserver names the annotation event.
    fn reaction_response(event_id: &str) -> HttpResponse {
        json_response(200, json!({ "event_id": event_id }))
    }

    /// A successful redaction: the homeserver names the redaction event.
    fn redaction_response() -> HttpResponse {
        json_response(200, json!({ "event_id": "$redact1" }))
    }

    #[tokio::test]
    async fn supports_reactions_is_true() {
        let mock = MockTransport::with(Vec::new());
        assert!(adapter(&mock).supports_reactions());
        assert_eq!(
            mock.request_count(),
            0,
            "the flag is a capability, not a call"
        );
    }

    #[tokio::test]
    async fn add_reaction_puts_an_m_annotation_event() {
        let mock = MockTransport::with(vec![reaction_response(REACTION_EVENT)]);
        assert!(
            adapter(&mock)
                .add_reaction("!room:example.org", "$msg1", "\u{1f440}")
                .await
        );

        let requests = mock.requests();
        assert_eq!(requests.len(), 1);
        assert_eq!(requests[0].method, HttpMethod::Put);
        assert!(
            requests[0]
                .url
                .starts_with(&format!("{ROOM_PATH}/send/m.reaction/")),
            "url was {}",
            requests[0].url
        );
        assert!(
            txn_of(&requests[0]).starts_with(&format!("vikram-{}", std::process::id())),
            "the existing txn scheme is reused: {}",
            txn_of(&requests[0])
        );
        assert_eq!(
            header(&requests[0], "Authorization"),
            Some("Bearer test-token")
        );
        assert_eq!(
            header(&requests[0], "Content-Type"),
            Some("application/json")
        );
        // The reference's content, exactly: one m.relates_to annotation relation.
        let body = body_json(&requests[0]);
        assert_eq!(body["m.relates_to"]["rel_type"], "m.annotation");
        assert_eq!(body["m.relates_to"]["event_id"], "$msg1");
        assert_eq!(body["m.relates_to"]["key"], "\u{1f440}");
        assert_eq!(
            body.as_object().map(|object| object.len()),
            Some(1),
            "the reference sends m.relates_to and nothing else: {body}"
        );
    }

    #[tokio::test]
    async fn the_reaction_key_is_the_emoji_itself_trimmed() {
        let mock = MockTransport::with(vec![reaction_response("$r1"), reaction_response("$r2")]);
        let matrix = adapter(&mock);
        assert!(
            matrix
                .add_reaction("!room:example.org", "$msg1", "  \u{2705}  ")
                .await
        );
        // Matrix keys are glyphs: a Slack-style name is not translated, it is the key verbatim.
        assert!(
            matrix
                .add_reaction("!room:example.org", "$msg1", "white_check_mark")
                .await
        );

        let requests = mock.requests();
        assert_eq!(body_json(&requests[0])["m.relates_to"]["key"], "\u{2705}");
        assert_eq!(
            body_json(&requests[1])["m.relates_to"]["key"],
            "white_check_mark"
        );
        assert_ne!(
            txn_of(&requests[0]),
            txn_of(&requests[1]),
            "two annotations must never share a transaction id"
        );
    }

    #[tokio::test]
    async fn remove_reaction_redacts_the_stored_reaction_event() {
        let mock = MockTransport::with(vec![
            reaction_response(REACTION_EVENT),
            redaction_response(),
        ]);
        let matrix = adapter(&mock);
        assert!(
            matrix
                .add_reaction("!room:example.org", "$msg1", "\u{1f440}")
                .await
        );
        assert!(
            matrix
                .remove_reaction("!room:example.org", "$msg1", "\u{1f440}")
                .await
        );

        let requests = mock.requests();
        assert_eq!(requests.len(), 2);
        let redaction = &requests[1];
        assert_eq!(redaction.method, HttpMethod::Put);
        assert!(
            redaction
                .url
                .starts_with(&format!("{ROOM_PATH}/redact/%24react1/")),
            "the redaction must target the reaction event the add created: {}",
            redaction.url
        );
        assert!(
            txn_of(redaction).starts_with(&format!("vikram-{}", std::process::id())),
            "the redaction uses the same txn scheme: {}",
            txn_of(redaction)
        );
        assert_eq!(
            header(redaction, "Authorization"),
            Some("Bearer test-token")
        );
        assert_eq!(
            body_json(redaction),
            json!({}),
            "the reference redacts without a reason"
        );

        // The annotation is gone, so a second removal refuses without a request.
        assert!(
            !matrix
                .remove_reaction("!room:example.org", "$msg1", "\u{1f440}")
                .await
        );
        assert_eq!(mock.request_count(), 2);
    }

    #[tokio::test]
    async fn removal_without_a_known_reaction_event_is_refused() {
        // Nothing was ever added: no id to redact, so nothing is sent.
        let mock = MockTransport::with(Vec::new());
        let matrix = adapter(&mock);
        assert!(
            !matrix
                .remove_reaction("!room:example.org", "$msg1", "\u{1f440}")
                .await
        );
        assert_eq!(mock.request_count(), 0);

        // A key that was never added, and a message that was never reacted to, are both refused.
        let mock = MockTransport::with(vec![reaction_response(REACTION_EVENT)]);
        let matrix = adapter(&mock);
        assert!(
            matrix
                .add_reaction("!room:example.org", "$msg1", "\u{1f440}")
                .await
        );
        assert!(
            !matrix
                .remove_reaction("!room:example.org", "$msg1", "\u{2705}")
                .await,
            "an unknown key must not redact the eyes annotation"
        );
        assert!(
            !matrix
                .remove_reaction("!room:example.org", "$msg2", "\u{1f440}")
                .await,
            "another message has no annotation of ours"
        );
        assert_eq!(mock.request_count(), 1, "only the add hit the wire");
    }

    #[tokio::test]
    async fn reaction_failures_are_false_and_record_nothing() {
        for reply in [
            json_response(
                403,
                json!({"errcode": "M_FORBIDDEN", "error": "no reactions here"}),
            ),
            json_response(
                429,
                json!({"errcode": "M_LIMIT_EXCEEDED", "retry_after_ms": 100}),
            ),
            // Accepted but unnamed: the event id is the only handle on the annotation.
            json_response(200, json!({})),
        ] {
            let mock = MockTransport::with(vec![reply]);
            let matrix = adapter(&mock);
            assert!(
                !matrix
                    .add_reaction("!room:example.org", "$msg1", "\u{1f440}")
                    .await
            );
            assert_eq!(mock.request_count(), 1);
            // A failed add records no event id, so a removal must not invent one.
            assert!(
                !matrix
                    .remove_reaction("!room:example.org", "$msg1", "\u{1f440}")
                    .await
            );
            assert_eq!(mock.request_count(), 1);
        }

        let offline = MockTransport::failing(TransportError::Request("down".into()));
        assert!(
            !adapter(&offline)
                .add_reaction("!room:example.org", "$msg1", "\u{1f440}")
                .await
        );
    }

    #[tokio::test]
    async fn a_failed_redaction_keeps_the_reaction_for_another_attempt() {
        let mock = MockTransport::with(vec![
            reaction_response(REACTION_EVENT),
            json_response(403, json!({"errcode": "M_FORBIDDEN"})),
            redaction_response(),
        ]);
        let matrix = adapter(&mock);
        assert!(
            matrix
                .add_reaction("!room:example.org", "$msg1", "\u{1f440}")
                .await
        );
        assert!(
            !matrix
                .remove_reaction("!room:example.org", "$msg1", "\u{1f440}")
                .await
        );
        // The annotation is still on the message, so the retry targets the same reaction event.
        assert!(
            matrix
                .remove_reaction("!room:example.org", "$msg1", "\u{1f440}")
                .await
        );
        assert!(
            mock.requests()[2].url.contains("/redact/%24react1/"),
            "url was {}",
            mock.requests()[2].url
        );
        assert_eq!(mock.request_count(), 3);
    }

    #[tokio::test]
    async fn blank_reaction_inputs_make_no_request() {
        let mock = MockTransport::with(Vec::new());
        let matrix = adapter(&mock);
        for (chat_id, message_id, emoji) in [
            ("", "$msg1", "\u{1f440}"),
            ("   ", "$msg1", "\u{1f440}"),
            ("!room:example.org", "", "\u{1f440}"),
            ("!room:example.org", " \t", "\u{1f440}"),
            ("!room:example.org", "$msg1", ""),
            ("!room:example.org", "$msg1", "   "),
        ] {
            assert!(
                !matrix.add_reaction(chat_id, message_id, emoji).await,
                "add {chat_id:?}/{message_id:?}/{emoji:?}"
            );
            assert!(
                !matrix.remove_reaction(chat_id, message_id, emoji).await,
                "remove {chat_id:?}/{message_id:?}/{emoji:?}"
            );
        }
        assert_eq!(mock.request_count(), 0, "blank input must not hit the wire");
    }

    // ----- outbound media (upload + m.room.message) -----

    /// Prefix of every upload URL this adapter builds; the `filename` query follows.
    const UPLOAD_URL: &str = "https://matrix.example.org/_matrix/media/v3/upload?filename=";

    /// The bytes every media fixture uploads.
    const MEDIA_BYTES: &[u8] = b"PNGDATA";

    fn media_bytes(kind: MediaKind) -> OutboundMedia {
        OutboundMedia::new(kind, MediaSource::Bytes(MEDIA_BYTES.to_vec()))
    }

    /// A successful upload answer carrying `content_uri`.
    fn upload_response(content_uri: &str) -> HttpResponse {
        json_response(200, json!({ "content_uri": content_uri }))
    }

    /// The two-request script a delivered attachment needs: the upload, then the send.
    fn delivered_media(content_uri: &str, event_id: &str) -> Vec<HttpResponse> {
        vec![
            upload_response(content_uri),
            json_response(200, json!({ "event_id": event_id })),
        ]
    }

    #[tokio::test]
    async fn supports_media_is_true() {
        let mock = MockTransport::with(Vec::new());
        assert!(adapter(&mock).supports_media());
        assert_eq!(
            mock.request_count(),
            0,
            "the flag is a capability, not a call"
        );
    }

    #[tokio::test]
    async fn upload_posts_the_bytes_then_sends_the_media_event() {
        let mock = MockTransport::with(delivered_media("mxc://example.org/abc", "$media1"));
        let media = media_bytes(MediaKind::Image)
            .with_filename("cat.png")
            .with_mime("image/png");
        let result = adapter(&mock)
            .send_media("!room:example.org", &media, None)
            .await;
        assert!(result.success, "{:?}", result.error);
        assert_eq!(result.message_id.as_deref(), Some("$media1"));

        let requests = mock.requests();
        assert_eq!(requests.len(), 2, "one upload, then one send");

        let upload = &requests[0];
        assert_eq!(upload.method, HttpMethod::Post);
        assert_eq!(upload.url, format!("{UPLOAD_URL}cat.png"));
        assert_eq!(header(upload, "Content-Type"), Some("image/png"));
        assert_eq!(
            header(upload, "Authorization"),
            Some("Bearer test-token"),
            "uploads are authenticated too"
        );
        assert_eq!(upload.body.as_deref(), Some(MEDIA_BYTES));

        let send = &requests[1];
        assert_eq!(send.method, HttpMethod::Put);
        assert!(
            send.url.starts_with(
                "https://matrix.example.org/_matrix/client/v3/rooms/%21room%3Aexample.org/send/m.room.message/"
            ),
            "url was {}",
            send.url
        );
        assert!(
            txn_of(send).starts_with(&format!("vikram-{}", std::process::id())),
            "the existing txn scheme is reused: {}",
            txn_of(send)
        );
        let body = body_json(send);
        assert_eq!(body["msgtype"], "m.image");
        assert_eq!(body["body"], "cat.png");
        assert_eq!(body["url"], "mxc://example.org/abc");
        assert_eq!(body["info"]["mimetype"], "image/png");
        assert_eq!(body["info"]["size"], MEDIA_BYTES.len());
        assert!(
            body["info"].get("w").is_none() && body["info"].get("h").is_none(),
            "the reference sends no dimensions on the plain upload path: {body}"
        );
        assert!(body.get("m.relates_to").is_none());
    }

    #[tokio::test]
    async fn upload_uses_the_filename_query_and_per_kind_defaults() {
        // An explicit filename is percent-encoded into the query.
        let mock = MockTransport::with(delivered_media("mxc://example.org/1", "$e1"));
        let media = media_bytes(MediaKind::Image).with_filename("my cat.png");
        assert!(
            adapter(&mock)
                .send_media("!room:example.org", &media, None)
                .await
                .success
        );
        assert_eq!(mock.requests()[0].url, format!("{UPLOAD_URL}my%20cat.png"));

        // Without a filename or a MIME, each kind falls back to its own pair.
        for (kind, msgtype, filename, mime) in [
            (MediaKind::Image, "m.image", "image.png", "image/png"),
            (MediaKind::Video, "m.video", "video.mp4", "video/mp4"),
            (MediaKind::Audio, "m.audio", "audio.mp3", "audio/mpeg"),
            (
                MediaKind::Document,
                "m.file",
                "file",
                "application/octet-stream",
            ),
        ] {
            let mock = MockTransport::with(delivered_media("mxc://example.org/2", "$e2"));
            let media = media_bytes(kind);
            let result = adapter(&mock)
                .send_media("!room:example.org", &media, None)
                .await;
            assert!(result.success, "{msgtype}: {:?}", result.error);

            let requests = mock.requests();
            assert_eq!(
                requests[0].url,
                format!("{UPLOAD_URL}{filename}"),
                "{msgtype}"
            );
            assert_eq!(
                header(&requests[0], "Content-Type"),
                Some(mime),
                "{msgtype}"
            );
            let body = body_json(&requests[1]);
            assert_eq!(body["msgtype"], msgtype);
            assert_eq!(body["body"], filename, "the body defaults to the filename");
            assert_eq!(body["info"]["mimetype"], mime);
        }

        // A filename with a known extension supplies the MIME when the caller passed none.
        let mock = MockTransport::with(delivered_media("mxc://example.org/3", "$e3"));
        let media = media_bytes(MediaKind::Document).with_filename("report.pdf");
        assert!(
            adapter(&mock)
                .send_media("!room:example.org", &media, None)
                .await
                .success
        );
        assert_eq!(
            header(&mock.requests()[0], "Content-Type"),
            Some("application/pdf")
        );
    }

    #[tokio::test]
    async fn caption_becomes_the_body_and_the_reply_relation_is_attached() {
        let mock = MockTransport::with(delivered_media("mxc://example.org/abc", "$media2"));
        let media = media_bytes(MediaKind::Image)
            .with_filename("cat.png")
            .with_caption("  look at this  ");
        let result = adapter(&mock)
            .send_media("!room:example.org", &media, Some("$orig"))
            .await;
        assert!(result.success, "{:?}", result.error);

        // The upload still names the file; only the event body carries the caption.
        assert_eq!(mock.requests()[0].url, format!("{UPLOAD_URL}cat.png"));
        let body = body_json(&mock.requests()[1]);
        assert_eq!(body["body"], "look at this");
        assert_eq!(body["url"], "mxc://example.org/abc");
        assert_eq!(body["msgtype"], "m.image");
        assert_eq!(body["m.relates_to"]["m.in_reply_to"]["event_id"], "$orig");

        // A blank caption is no caption: the filename is the body.
        let blank = MockTransport::with(delivered_media("mxc://example.org/def", "$media3"));
        let media = media_bytes(MediaKind::Image)
            .with_filename("cat.png")
            .with_caption("   ");
        assert!(
            adapter(&blank)
                .send_media("!room:example.org", &media, None)
                .await
                .success
        );
        assert_eq!(body_json(&blank.requests()[1])["body"], "cat.png");
    }

    #[tokio::test]
    async fn an_upload_without_content_uri_is_a_failure_with_no_send() {
        let mock = MockTransport::with(vec![
            json_response(200, json!({})),
            json_response(200, json!({ "event_id": "$unused" })),
        ]);
        let result = adapter(&mock)
            .send_media("!room:example.org", &media_bytes(MediaKind::Image), None)
            .await;
        assert!(!result.success);
        assert_eq!(result.error_kind.as_deref(), Some("unknown"));
        assert!(
            result
                .error
                .as_deref()
                .unwrap_or_default()
                .contains("content_uri"),
            "the missing field should be named: {:?}",
            result.error
        );
        assert_eq!(mock.request_count(), 1, "no send without a content URI");

        // A blank content URI is not a content URI either.
        let blank = MockTransport::with(vec![upload_response("   ")]);
        let result = adapter(&blank)
            .send_media("!room:example.org", &media_bytes(MediaKind::Image), None)
            .await;
        assert!(!result.success);
        assert_eq!(blank.request_count(), 1);
    }

    #[tokio::test]
    async fn upload_failure_stops_before_the_send() {
        let mock = MockTransport::with(vec![json_response(
            413,
            json!({"errcode": "M_TOO_LARGE", "error": "too big"}),
        )]);
        let result = adapter(&mock)
            .send_media("!room:example.org", &media_bytes(MediaKind::Image), None)
            .await;
        assert!(!result.success);
        assert_eq!(result.message_id, None);
        assert_eq!(result.error_kind.as_deref(), Some("too_long"));
        assert_eq!(
            mock.request_count(),
            1,
            "the upload failed, so no send was attempted"
        );

        // The same holds for a transport-level upload failure.
        let offline = MockTransport::failing(TransportError::Request("connection reset".into()));
        let result = adapter(&offline)
            .send_media("!room:example.org", &media_bytes(MediaKind::Image), None)
            .await;
        assert!(!result.success);
        assert_eq!(result.error_kind.as_deref(), Some("transient"));
        assert_eq!(offline.request_count(), 1);
    }

    #[tokio::test]
    async fn a_send_failure_is_surfaced_never_reported_as_delivered() {
        let mock = MockTransport::with(vec![
            upload_response("mxc://example.org/abc"),
            json_response(500, json!({"error": "boom"})),
        ]);
        let result = adapter(&mock)
            .send_media("!room:example.org", &media_bytes(MediaKind::Image), None)
            .await;
        assert!(!result.success, "an uploaded file is not a delivered one");
        assert_eq!(result.message_id, None);
        assert_eq!(result.error_kind.as_deref(), Some("transient"));
        assert!(result.retryable);
        assert_eq!(
            mock.request_count(),
            2,
            "the upload did run, the send failed"
        );

        // A 200 with no event_id is delivered-but-unidentified, reported as unknown.
        let unidentified = MockTransport::with(vec![
            upload_response("mxc://example.org/abc"),
            json_response(200, json!({})),
        ]);
        let result = adapter(&unidentified)
            .send_media("!room:example.org", &media_bytes(MediaKind::Image), None)
            .await;
        assert!(!result.success);
        assert_eq!(result.error_kind.as_deref(), Some("unknown"));
    }

    #[tokio::test]
    async fn media_failures_map_to_the_standard_kinds() {
        for (status, body, kind, retryable) in [
            (
                413u16,
                json!({"errcode": "M_TOO_LARGE", "error": "too big"}),
                "too_long",
                false,
            ),
            (
                429,
                json!({
                    "errcode": "M_LIMIT_EXCEEDED",
                    "error": "slow down",
                    "retry_after_ms": 2500,
                }),
                "rate_limited",
                true,
            ),
            (
                403,
                json!({"errcode": "M_FORBIDDEN", "error": "no luck"}),
                "forbidden",
                false,
            ),
            (
                401,
                json!({"errcode": "M_UNKNOWN_TOKEN", "error": "bad token"}),
                "forbidden",
                false,
            ),
            (
                404,
                json!({"errcode": "M_NOT_FOUND", "error": "gone"}),
                "not_found",
                false,
            ),
            (500, json!({"error": "boom"}), "transient", true),
            (503, json!({"error": "down"}), "transient", true),
        ] {
            let mock = MockTransport::with(vec![json_response(status, body)]);
            let result = adapter(&mock)
                .send_media("!room:example.org", &media_bytes(MediaKind::Image), None)
                .await;
            assert!(!result.success, "{status}");
            assert_eq!(result.error_kind.as_deref(), Some(kind), "{status}");
            assert_eq!(result.retryable, retryable, "{status}");
            assert_eq!(mock.request_count(), 1, "{status}");
        }

        // The rate limit keeps the body's delay, converted from milliseconds.
        let limited = MockTransport::with(vec![json_response(
            429,
            json!({"errcode": "M_LIMIT_EXCEEDED", "retry_after_ms": 2500}),
        )]);
        let result = adapter(&limited)
            .send_media("!room:example.org", &media_bytes(MediaKind::Image), None)
            .await;
        assert_eq!(result.retry_after, Some(2.5));

        // A 429 whose delay is only in the Retry-After header is already in seconds.
        let header_only = MockTransport::with(vec![HttpResponse {
            status: 429,
            body: json!({"errcode": "M_LIMIT_EXCEEDED"}).to_string(),
            retry_after: Some(1.5),
        }]);
        let result = adapter(&header_only)
            .send_media("!room:example.org", &media_bytes(MediaKind::Image), None)
            .await;
        assert_eq!(result.retry_after, Some(1.5));

        // A transport error never produced a response: retryable, and no send followed.
        let offline = MockTransport::failing(TransportError::Request("offline".into()));
        let result = adapter(&offline)
            .send_media("!room:example.org", &media_bytes(MediaKind::Image), None)
            .await;
        assert_eq!(result.error_kind.as_deref(), Some("transient"));
        assert!(result.retryable);
        assert_eq!(offline.request_count(), 1);
    }

    #[tokio::test]
    async fn empty_media_is_rejected_without_a_request() {
        let mock = MockTransport::with(Vec::new());
        let media = OutboundMedia::new(MediaKind::Image, MediaSource::Bytes(Vec::new()));
        let result = adapter(&mock)
            .send_media("!room:example.org", &media, None)
            .await;
        assert!(!result.success);
        assert_eq!(result.error_kind.as_deref(), Some("bad_format"));
        assert_eq!(mock.request_count(), 0);
    }

    #[test]
    fn oversized_media_is_refused_locally_as_too_long() {
        // The guard `send_media` applies before uploading (hermes' `_media_too_large`).
        assert_eq!(MAX_MEDIA_BYTES, 100 * 1024 * 1024);
        assert!(size_failure(MAX_MEDIA_BYTES).is_none());
        let failure = size_failure(MAX_MEDIA_BYTES + 1).expect("over the cap");
        assert_eq!(failure.error_kind.as_deref(), Some("too_long"));
        assert!(!failure.success);
        assert_eq!(
            failure.error.as_deref(),
            Some(
                format!(
                    "Media file exceeds Matrix limit ({} > {MAX_MEDIA_BYTES} bytes)",
                    MAX_MEDIA_BYTES + 1
                )
                .as_str()
            ),
            "hermes' wording is kept"
        );
    }

    #[tokio::test]
    async fn url_media_is_refused_with_a_reason_like_the_reference() {
        // hermes downloads http(s) media itself and refuses a non-mxc `url` on the inbound path;
        // this port has no fetcher, so both URL shapes are refused before any request.
        for url in ["https://example.test/cat.png", "mxc://example.org/abc"] {
            let mock = MockTransport::with(Vec::new());
            let media = OutboundMedia::new(MediaKind::Image, MediaSource::Url(url.to_string()));
            let result = adapter(&mock)
                .send_media("!room:example.org", &media, None)
                .await;
            assert!(!result.success, "{url}");
            assert_eq!(result.error_kind.as_deref(), Some("bad_format"), "{url}");
            let error = result.error.as_deref().unwrap_or_default();
            assert!(error.contains("not supported"), "{url}: {error}");
            assert!(error.contains("MediaSource::Bytes"), "{url}: {error}");
            assert_eq!(mock.request_count(), 0, "{url}");
        }

        // The two shapes explain themselves differently.
        let http = url_refusal("https://example.test/cat.png");
        assert!(
            http.error
                .as_deref()
                .unwrap_or_default()
                .contains("no URL fetcher")
        );
        let mxc = url_refusal("mxc://example.org/abc");
        assert!(mxc.error.as_deref().unwrap_or_default().contains("mxc://"));
    }

    #[tokio::test]
    async fn uploads_follow_the_api_base_override() {
        let mock = MockTransport::with(delivered_media("mxc://example.org/abc", "$e"));
        let matrix = MatrixAdapter::new(mock.clone(), "https://ignored.example.org", "t")
            .with_api_base("http://localhost:8008/_matrix/client/v3/");
        assert!(
            matrix
                .send_media("!room:example.org", &media_bytes(MediaKind::Image), None)
                .await
                .success
        );
        assert_eq!(
            mock.requests()[0].url,
            "http://localhost:8008/_matrix/media/v3/upload?filename=image.png"
        );
        assert!(
            mock.requests()[1]
                .url
                .starts_with("http://localhost:8008/_matrix/client/v3/rooms/"),
            "url was {}",
            mock.requests()[1].url
        );
    }

    #[test]
    fn media_helpers_pick_names_mimes_bodies_and_errors() {
        assert_eq!(msgtype_for(MediaKind::Image), "m.image");
        assert_eq!(msgtype_for(MediaKind::Video), "m.video");
        assert_eq!(msgtype_for(MediaKind::Audio), "m.audio");
        assert_eq!(msgtype_for(MediaKind::Document), "m.file");

        // Filenames: the caller's, trimmed; otherwise the per-kind default.
        let image = media_bytes(MediaKind::Image).with_filename(" cat.png ");
        assert_eq!(media_filename(&image), "cat.png");
        assert_eq!(media_filename(&media_bytes(MediaKind::Document)), "file");
        assert_eq!(
            media_filename(&media_bytes(MediaKind::Audio).with_filename("  ")),
            "audio.mp3"
        );

        // MIMEs: the caller's, else the extension, else the per-kind default.
        let explicit = media_bytes(MediaKind::Image).with_mime("image/avif");
        assert_eq!(media_mime(&explicit, "image.png"), "image/avif");
        assert_eq!(
            media_mime(&media_bytes(MediaKind::Image), "cat.webp"),
            "image/webp"
        );
        assert_eq!(
            media_mime(&media_bytes(MediaKind::Video), "clip.mkv"),
            "video/x-matroska"
        );
        assert_eq!(
            media_mime(&media_bytes(MediaKind::Video), "clip.unknown"),
            "video/mp4",
            "an unknown extension falls back to the kind default"
        );
        assert_eq!(
            media_mime(&media_bytes(MediaKind::Document), "no-extension"),
            "application/octet-stream"
        );

        // Bodies: caption wins, filename otherwise.
        let captioned = media_bytes(MediaKind::Image).with_caption("hi");
        assert_eq!(media_body(&captioned, "cat.png"), "hi");
        assert_eq!(
            media_body(&media_bytes(MediaKind::Image), "cat.png"),
            "cat.png"
        );

        // content_uri parsing trims and rejects blanks.
        assert_eq!(
            content_uri_from(&json!({"content_uri": " mxc://example.org/a "})).as_deref(),
            Some("mxc://example.org/a")
        );
        assert_eq!(content_uri_from(&json!({"content_uri": "  "})), None);
        assert_eq!(content_uri_from(&json!({"other": 1})), None);
        assert_eq!(content_uri_from(&Value::Null), None);

        // The payload carries exactly the reference's fields.
        let payload = media_payload("m.image", "cat.png", "mxc://a/b", "image/png", 7, None);
        assert_eq!(payload["msgtype"], "m.image");
        assert_eq!(payload["body"], "cat.png");
        assert_eq!(payload["url"], "mxc://a/b");
        assert_eq!(payload["info"], json!({"mimetype": "image/png", "size": 7}));
        assert!(payload.get("m.relates_to").is_none());

        // 413 / M_TOO_LARGE is the media-only addition; everything else is the shared taxonomy.
        assert_eq!(
            classify_media_failure(&json_response(413, json!({"error": "big"})))
                .error_kind
                .as_deref(),
            Some("too_long")
        );
        assert_eq!(
            classify_media_failure(&json_response(400, json!({"errcode": "M_TOO_LARGE"})))
                .error_kind
                .as_deref(),
            Some("too_long"),
            "the errcode alone is enough"
        );
        assert_eq!(
            classify_media_failure(&json_response(
                400,
                json!({"errcode": "M_FORBIDDEN", "error": "no"})
            ))
            .error_kind
            .as_deref(),
            Some("forbidden")
        );
        assert!(
            !classify_media_failure(&json_response(413, json!({"error": "big"}))).retryable,
            "the same bytes will never fit"
        );
    }

    #[test]
    fn helpers_classify_media_and_names() {
        assert_eq!(message_type_for("m.text"), Some(MessageType::Text));
        assert_eq!(message_type_for("m.notice"), Some(MessageType::Text));
        assert_eq!(message_type_for("m.image"), Some(MessageType::Photo));
        assert_eq!(message_type_for("m.file"), Some(MessageType::Document));
        assert_eq!(message_type_for("m.audio"), Some(MessageType::Audio));
        assert_eq!(message_type_for("m.video"), Some(MessageType::Video));
        assert_eq!(message_type_for("m.emote"), None);
        assert_eq!(message_type_for(""), None);

        assert_eq!(display_name_from_mxid("@alice:example.org"), "alice");
        assert_eq!(display_name_from_mxid("alice"), "alice");
        assert_eq!(display_name_from_mxid("@:example.org"), "@:example.org");

        assert_eq!(percent_encode("!room:example.org"), "%21room%3Aexample.org");
        assert_eq!(percent_encode("s1_2-3.4~5"), "s1_2-3.4~5");
        assert_eq!(body_excerpt("   "), "<empty body>");
        assert_eq!(body_excerpt(&"x".repeat(300)).chars().count(), 201);
        assert_eq!(
            classify_failure(&json_response(400, json!({"error": "x"})))
                .error_kind
                .as_deref(),
            Some("bad_format")
        );
    }
}
