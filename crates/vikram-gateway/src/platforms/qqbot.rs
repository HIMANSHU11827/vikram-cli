//! QQ Bot adapter — Tencent QQ Open Platform (official Bot API v2, HTTP half).
//!
//! Rust port of the outbound and inbound-normalization half of hermes-agent
//! `plugins/platforms/qqbot/adapter.py` plus `plugins/platforms/qqbot/constants.py`. Everything
//! hermes builds on `httpx`/`aiohttp` is re-implemented here directly over
//! [`crate::transport::Transport`], so the adapter stays testable with no network, no app
//! credentials and no SDK.
//!
//! Ported behaviour, with the hermes origin:
//! - App access token: `POST https://bots.qq.com/app/getAppAccessToken` with
//!   `{"appId", "clientSecret"}`, cached for `expires_in − 60` seconds and reused across sends
//!   (`_ensure_token`, `_token_fresh`, `TOKEN_URL`).
//! - Outbound text: `POST /channels/{id}/messages` for guild channels,
//!   `POST /v2/groups/{id}/messages` for group openids and `POST /v2/users/{id}/messages` for C2C
//!   users (`_send_guild_text`, `_messages_path`, `_send_text_to`), with the `QQBot <token>`
//!   `Authorization` header (`_auth_headers`).
//! - Body shape `{"content", "msg_type": 0}` (`MSG_TYPE_TEXT`), plus `msg_id`/`msg_seq` for the
//!   passive-reply endpoints (`_build_text_body`, `_send_text_to`).
//! - [`QQ_MAX_MESSAGE_LENGTH`] = 4000-code-point chunking through [`truncate_message`], chunks
//!   delivered in order with the last id as `message_id` (`send`, `MAX_MESSAGE_LENGTH`).
//! - Send failure taxonomy: `429` / body code 11244 / 11245 → `rate_limited` (retryable, honouring
//!   `Retry-After`), `401`/`403` / codes 11241 / 11242 → `forbidden` (dropping the token cache on
//!   11241), `404` → `not_found`, other 4xx → `bad_format`, 5xx and transport errors →
//!   `transient` (retryable).
//! - Inbound dispatch normalization: `AT_MESSAGE_CREATE` (guild), `GROUP_AT_MESSAGE_CREATE` and
//!   `C2C_MESSAGE_CREATE`, with the leading `<@!…>`/`<@…>` mention stripped like hermes'
//!   `_strip_at_mention` (`_handle_guild_message`, `_handle_group_message`, `_handle_c2c_message`).
//! - Outbound media for URL sources ([`PlatformAdapter::send_media`]): one `POST .../files` with
//!   `url=` and `srv_send_msg: false` — plus `file_name` for documents — then one RichMedia
//!   message `{"msg_type": 7, "media": {"file_info": …}, "msg_seq": …, "content"?: caption,
//!   "msg_id"?: reply_to}` (`_send_media`, `_upload_media`, `_rest_path`, `MSG_TYPE_MEDIA`,
//!   `MEDIA_TYPE_*`). Guild channels are refused exactly as the reference refuses them, and bytes
//!   are refused rather than faked (see [`PlatformAdapter::send_media`]).
//!
//! Deliberately not ported (each needs process state, a media pipeline or a long-lived socket this
//! stateless wire adapter does not own):
//! - the WebSocket gateway client: `/gateway` discovery, `op 2` Identify / `op 6` Resume, the
//!   `op 10` Hello heartbeat at 80 % of `heartbeat_interval`, `op 7`/`op 9` handling, close-code
//!   reconnect ladders and quick-disconnect detection. The host owns the socket and feeds each
//!   dispatch frame to [`QqBotAdapter::parse_webhook`];
//! - the chunked local-file uploader (`chunked_upload.py`: `POST .../upload_prepare`, a pre-signed
//!   COS `PUT` per block, `POST .../upload_part_finish`, then `POST .../files` with the resulting
//!   `upload_id`), which is why [`MediaSource::Bytes`] attachments are refused instead of being
//!   faked; QQ's free `asr_refer_text` and the SILK→WAV/ffmpeg/STT voice pipeline;
//! - inline keyboards, `INTERACTION_CREATE` ACK + routing, approval/update-prompt cards;
//! - `dm_policy` / `group_policy` ACLs, pairing, `MessageDeduplicator`, per-chat typing debounce and
//!   the last-inbound-msg-id table — the runner owns admission and per-chat state;
//! - quoted-message (`message_type: 103` + `msg_elements`) folding and attachment transcript
//!   folding into the event text.
//!
//! QQ's OpenAPI exposes no chat-info lookup, no message edit and no message delete for bot
//! messages, so those three surface operations are honest no-ops / refusals here rather than
//! invented endpoints.
//!
//! One thing this adapter has that hermes' does not: [`QqBotAdapter::verify_webhook`], which checks
//! the Ed25519 signature QQ's HTTP webhook mode puts on every delivery, keyed by the bot's
//! `client_secret` (see that method for the scheme). Verification is separate from parsing —
//! [`QqBotAdapter::parse_webhook`] stays a pure frame parser — and replay windows / frame
//! deduplication remain the host's job.

use async_trait::async_trait;
use ring::signature::{self, Ed25519KeyPair, KeyPair, UnparsedPublicKey};
use serde_json::{Value, json};
use std::sync::{Mutex, MutexGuard};
use std::time::{Duration, Instant};

use crate::event::{MessageEvent, Platform, SessionSource, platform_names};
use crate::helpers::{codepoint_len, truncate_message};
use crate::platform::{
    ChatInfo, MediaKind, MediaSource, OutboundMedia, PlatformAdapter, SendResult,
};
use crate::transport::{HttpRequest, SharedTransport};

/// QQ Open Platform origin for the bot REST API (hermes `API_BASE`).
pub const QQ_API_BASE: &str = "https://api.sgroup.qq.com";

/// The app-access-token endpoint (hermes `TOKEN_URL`). A separate host from [`QQ_API_BASE`], and
/// deliberately not derived from `api_base`: `with_api_base` only redirects the message API.
pub const QQ_TOKEN_URL: &str = "https://bots.qq.com/app/getAppAccessToken";

/// QQ's per-message text cap in code points (hermes `MAX_MESSAGE_LENGTH`); longer answers are
/// chunked by [`QqBotAdapter::send`].
pub const QQ_MAX_MESSAGE_LENGTH: usize = 4000;

/// A token is renewed this long before QQ says it expires, so an in-flight send never races the
/// deadline (hermes `_token_fresh`).
const TOKEN_SAFETY_MARGIN_SECS: i64 = 60;

/// `expires_in` assumed when the token response omits it (hermes' default of two hours).
const DEFAULT_TOKEN_TTL_SECS: i64 = 7200;

/// `msg_type` for a plain text message (hermes `MSG_TYPE_TEXT`).
const MSG_TYPE_TEXT: i64 = 0;

/// `msg_type` for a RichMedia message — a message that carries an uploaded attachment
/// (hermes `MSG_TYPE_MEDIA`).
const MSG_TYPE_MEDIA: i64 = 7;

/// `/files` upload `file_type` for an image (hermes `MEDIA_TYPE_IMAGE`).
const MEDIA_TYPE_IMAGE: i64 = 1;

/// `/files` upload `file_type` for a video (hermes `MEDIA_TYPE_VIDEO`).
const MEDIA_TYPE_VIDEO: i64 = 2;

/// `/files` upload `file_type` for a voice clip (hermes `MEDIA_TYPE_VOICE`).
const MEDIA_TYPE_VOICE: i64 = 3;

/// `/files` upload `file_type` for a document (hermes `MEDIA_TYPE_FILE`). This is the only type
/// that also carries `file_name` in the upload body.
const MEDIA_TYPE_FILE: i64 = 4;

/// The `msg_seq` stamped on a passive reply. hermes' `_next_msg_seq` derives a pseudo-random
/// `0..65535` value so repeated passive replies to one `msg_id` stay distinct; this adapter sends
/// at most one reply per turn, so the same fixed `1` that [`text_body`] stamps is enough — and a
/// constant keeps the wire body deterministic for tests.
const PASSIVE_MSG_SEQ: i64 = 1;

/// QQ codes meaning "you are sending too fast" (`11244` group/C2C, `11245` guild frequency limit).
const RATE_LIMIT_CODES: [i64; 2] = [11_244, 11_245];

/// QQ code meaning "token does not exist or has expired" — the cached token is worthless after it.
const TOKEN_INVALID_CODE: i64 = 11_241;

/// QQ codes that blame the token: `11241` (missing/expired) and `11242` (verification failed).
const TOKEN_REJECTED_CODES: [i64; 2] = [11_241, 11_242];

/// Guild channel `@`-message dispatch.
const GUILD_MESSAGE_EVENT: &str = "AT_MESSAGE_CREATE";

/// Group `@`-message dispatch.
const GROUP_MESSAGE_EVENT: &str = "GROUP_AT_MESSAGE_CREATE";

/// C2C (one-to-one) message dispatch.
const C2C_MESSAGE_EVENT: &str = "C2C_MESSAGE_CREATE";

/// An Ed25519 seed is exactly 32 bytes, and QQ's webhook signing key is derived from the bot's
/// `client_secret` used verbatim as that seed.
const ED25519_SEED_LEN: usize = 32;

/// An Ed25519 signature is `<R ‖ S>`: two 32-byte halves, 64 bytes in total.
const ED25519_SIGNATURE_LEN: usize = 64;

/// One half of an Ed25519 signature — a compressed point (`R`, `A`) or a scalar (`S`, `k`).
const ED25519_HALF_LEN: usize = 32;

/// The Ed25519 group order `L = 2^252 + 27742317777372353535851937790883648493`, little-endian
/// (RFC 8032 §5.1) — the bound a canonical `S` stays under.
const ED25519_ORDER_L: [u8; ED25519_HALF_LEN] = [
    0xed, 0xd3, 0xf5, 0x5c, 0x1a, 0x63, 0x12, 0x58, 0xd6, 0x9c, 0xf7, 0xa2, 0xde, 0xf9, 0xde, 0x14,
    0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x10,
];

/// One app access token plus the instant it stops being usable.
#[derive(Debug, Clone)]
pub struct CachedToken {
    /// The `access_token` QQ issued.
    pub value: String,
    /// When the token must be replaced: issue time + `expires_in` − [`TOKEN_SAFETY_MARGIN_SECS`].
    pub expires_at: Instant,
}

/// Which of the three QQ bot message surfaces a chat id belongs to.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum ChatKind {
    /// A guild text channel: `POST /channels/{id}/messages`.
    Guild,
    /// A group openid: `POST /v2/groups/{id}/messages`.
    Group,
    /// A C2C user openid: `POST /v2/users/{id}/messages`.
    Dm,
}

impl ChatKind {
    /// The gateway's chat class for this surface (`guild` | `group` | `dm`), also the value
    /// carried in `metadata["chat_type"]` so a reply routes back to the same endpoint.
    fn as_str(self) -> &'static str {
        match self {
            Self::Guild => "guild",
            Self::Group => "group",
            Self::Dm => "dm",
        }
    }

    /// Read a caller-supplied `chat_type` hint. Unknown spellings fall through to id inference
    /// instead of guessing a surface.
    fn from_hint(value: &Value) -> Option<Self> {
        let raw = value.as_str()?.trim().to_ascii_lowercase();
        match raw.as_str() {
            "guild" | "channel" => Some(Self::Guild),
            "group" => Some(Self::Group),
            "dm" | "c2c" | "private" => Some(Self::Dm),
            _ => None,
        }
    }
}

/// QQ Bot Open Platform adapter.
///
/// Holds one shared [`crate::transport::Transport`], the app credentials and the access-token
/// cache. The cache is a single slot behind a [`Mutex`] so sends from several tasks reuse one
/// token; the mutex is never held across an `await`.
pub struct QqBotAdapter {
    transport: SharedTransport,
    app_id: String,
    client_secret: String,
    api_base: String,
    token: Mutex<Option<CachedToken>>,
}

impl QqBotAdapter {
    /// Build an adapter for `app_id`/`client_secret` over `transport`, talking to [`QQ_API_BASE`].
    pub fn new(
        transport: SharedTransport,
        app_id: impl Into<String>,
        client_secret: impl Into<String>,
    ) -> Self {
        Self {
            transport,
            app_id: app_id.into(),
            client_secret: client_secret.into(),
            api_base: QQ_API_BASE.to_string(),
            token: Mutex::new(None),
        }
    }

    /// Point the message API at a mirror or a test double; a trailing `/` is dropped. The token
    /// endpoint stays at [`QQ_TOKEN_URL`].
    pub fn with_api_base(mut self, base: impl Into<String>) -> Self {
        self.api_base = base.into().trim_end_matches('/').to_string();
        self
    }

    /// The current app access token: the cached one while it is still live, otherwise a fresh one
    /// from [`QQ_TOKEN_URL`].
    ///
    /// Single-flight is not attempted (two concurrent misses may both fetch); the [`Mutex`] only
    /// guards the cache slot, which is what keeps later calls from re-fetching.
    pub async fn access_token(&self) -> Result<String, String> {
        self.acquire_token()
            .await
            .map_err(|failure| failure.message)
    }

    /// Normalize one gateway dispatch frame into events.
    ///
    /// Only `op: 0` dispatch frames of type `AT_MESSAGE_CREATE` (guild),
    /// `GROUP_AT_MESSAGE_CREATE` (group) or `C2C_MESSAGE_CREATE` (C2C/DM) produce an event.
    /// Everything else — Hello (`op 10`), heartbeats (`op 11`), `READY`/`RESUMED`, server
    /// reconnect (`op 7`), invalid session (`op 9`), `INTERACTION_CREATE`, other dispatch types —
    /// yields an empty vector, and so does a message with blank content.
    ///
    /// WebSocket delivery is the **host's** job: this adapter never opens a socket, so `op 10`'s
    /// `heartbeat_interval`, the `op 1` heartbeat cadence, `op 2` Identify / `op 6` Resume and the
    /// `seq`-based resume bookkeeping all belong to whoever owns the connection. This function
    /// parses exactly one already-received frame. Invalid JSON is an `Err`, because that means the
    /// host is not forwarding QQ frames at all.
    pub fn parse_webhook(&self, body: &str) -> Result<Vec<MessageEvent>, String> {
        let root: Value =
            serde_json::from_str(body).map_err(|err| format!("invalid json: {err}"))?;
        // `op 0` is the only frame that carries an event; Hello/heartbeat/ACK/control frames do not.
        if value_to_i64(root.get("op")) != Some(0) {
            return Ok(Vec::new());
        }
        let kind = match root.get("t").and_then(Value::as_str).unwrap_or_default() {
            GUILD_MESSAGE_EVENT => ChatKind::Guild,
            GROUP_MESSAGE_EVENT => ChatKind::Group,
            C2C_MESSAGE_EVENT => ChatKind::Dm,
            _ => return Ok(Vec::new()),
        };
        let Some(data) = root.get("d").filter(|data| data.is_object()) else {
            return Ok(Vec::new());
        };
        match parse_dispatch(kind, data) {
            Some(event) => Ok(vec![event]),
            None => Ok(Vec::new()),
        }
    }

    /// Whether this adapter's `client_secret` can be QQ's webhook signing seed at all: it must be
    /// exactly 32 bytes, because that is the length of an Ed25519 seed and QQ derives the key by
    /// using the `client_secret` verbatim as one.
    pub fn has_webhook_secret(&self) -> bool {
        self.client_secret.len() == ED25519_SEED_LEN
    }

    /// Verify one QQ webhook delivery against its signature headers.
    ///
    /// QQ's HTTP webhook mode signs every request with Ed25519, keyed by the bot's `client_secret`
    /// used **verbatim as the 32-byte signing seed** — there is no separate webhook public key to
    /// configure, and no HMAC secret. Everything below is RFC 8032 with that seed:
    ///
    /// 1. Key derivation (§5.1.5): `h = SHA-512(seed)`; clamp the first half
    ///    (`h[0] &= 248`, `h[31] &= 127`, `h[31] |= 64`); `a = h[0..32] mod L`; `A = [a]B`.
    /// 2. Message: `M = timestamp.as_bytes() ‖ body`, both exactly as received. The timestamp is
    ///    not trimmed or re-parsed and the body must be the raw bytes that were signed, so a
    ///    caller that re-serializes JSON before verifying will reject genuine frames.
    /// 3. Verification (§5.1.7): split the 64-byte signature into `R` and `S`;
    ///    `k = SHA-512(R ‖ A ‖ M) mod L`; `R_check = [S]B − [k]A`; accept when the compressed
    ///    `R_check` equals `R`.
    ///
    /// `signature_hex` is `<R ‖ S>` hex-encoded in either case; surrounding whitespace is ignored,
    /// since header values are often carried around with it.
    ///
    /// Every rejection is a refusal, never a repair — a verifier that "fixed" any of these would
    /// accept signatures the platform never issued:
    ///
    /// - a `client_secret` that is not exactly 32 bytes. Zero-padding a short secret (or
    ///   truncating a long one) would silently derive a *different* key: every genuine frame would
    ///   fail, and the adapter would be checking QQ's frames against a key nobody registered — so
    ///   a typo'd credential would look like an attack instead of a setup error;
    /// - a signature that is not hex, or not exactly 64 bytes once decoded;
    /// - a non-canonical scalar (`S >= L`). `S + L` is the same scalar modulo `L`, so a verifier
    ///   that reduced instead of rejecting would accept a mutilated signature;
    /// - an `R` (or a derived `A`) that is not a valid curve point.
    ///
    /// Nothing here unwraps, indexes or slices on wire data: every failure path returns `false`.
    pub fn verify_webhook(&self, timestamp: &str, signature_hex: &str, body: &[u8]) -> bool {
        // No padding: see the doc comment — a non-32-byte secret is a configuration error, and
        // inventing a key for it would hide that.
        let Ok(seed) = <&[u8; ED25519_SEED_LEN]>::try_from(self.client_secret.as_bytes()) else {
            return false;
        };
        let Some(decoded) = decode_hex_bytes(signature_hex.trim()) else {
            return false;
        };
        let Ok(signature) = <[u8; ED25519_SIGNATURE_LEN]>::try_from(decoded.as_slice()) else {
            return false;
        };
        // `S` is the second half. Rejecting out-of-range `S` up front is what separates this from a
        // verifier that would happily reduce `S + L` back to `S`.
        let mut scalar = [0u8; ED25519_HALF_LEN];
        scalar.copy_from_slice(&signature[ED25519_HALF_LEN..]);
        if !scalar_is_canonical(&scalar) {
            return false;
        }
        let Some(public_key) = webhook_public_key(seed) else {
            return false;
        };
        let mut message = Vec::with_capacity(timestamp.len() + body.len());
        message.extend_from_slice(timestamp.as_bytes());
        message.extend_from_slice(body);
        // `UnparsedPublicKey::verify` is RFC 8032 §5.1.7 as described above: it recomputes
        // `k = SHA-512(R ‖ A ‖ M) mod L`, evaluates `[S]B − [k]A`, compares the compression against
        // `R`, and rejects undecodable points instead of panicking.
        UnparsedPublicKey::new(&signature::ED25519, public_key)
            .verify(&message, &signature)
            .is_ok()
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

    /// Cache `value` for `ttl_secs`; a non-positive TTL stores nothing, so a short-lived token is
    /// re-fetched next time instead of being handed out already stale.
    fn store_token(&self, value: &str, ttl_secs: i64) {
        if ttl_secs <= 0 {
            return;
        }
        let ttl = Duration::from_secs(ttl_secs.unsigned_abs());
        let Some(expires_at) = Instant::now().checked_add(ttl) else {
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
        self.fetch_access_token().await
    }

    /// `POST {QQ_TOKEN_URL}` with `{"appId", "clientSecret"}`, caching `access_token` on success.
    async fn fetch_access_token(&self) -> Result<String, Failure> {
        let payload = json!({
            "appId": self.app_id,
            "clientSecret": self.client_secret,
        });
        let request = HttpRequest::json(QQ_TOKEN_URL, &payload);
        let response = self
            .transport
            .execute(request)
            .await
            .map_err(|err| Failure::transient(format!("qqbot token request failed: {err}")))?;
        let parsed = response.json().unwrap_or(Value::Null);
        if !response.ok() {
            return Err(credentials_failure(
                response.status,
                &parsed,
                response.retry_after,
            ));
        }
        let token = parsed
            .get("access_token")
            .and_then(Value::as_str)
            .map(str::trim)
            .filter(|token| !token.is_empty());
        let Some(token) = token else {
            return Err(Failure::unknown(
                "qqbot token response carried no access_token",
            ));
        };
        let expires_in = value_to_i64(parsed.get("expires_in")).unwrap_or(DEFAULT_TOKEN_TTL_SECS);
        self.store_token(token, expires_in.saturating_sub(TOKEN_SAFETY_MARGIN_SECS));
        Ok(token.to_string())
    }

    /// One chunk to the endpoint for `kind`, returning the `id` QQ assigned it.
    async fn send_chunk(
        &self,
        kind: ChatKind,
        chat_id: &str,
        chunk: &str,
        reply_to: Option<&str>,
    ) -> Result<String, Failure> {
        let token = self.acquire_token().await?;
        let url = format!("{}{}", self.api_base, messages_path(kind, chat_id));
        let request = HttpRequest::json(url, &text_body(kind, chunk, reply_to))
            .header("Authorization", format!("QQBot {token}"));
        let response = self
            .transport
            .execute(request)
            .await
            .map_err(|err| Failure::transient(format!("qqbot send transport error: {err}")))?;
        let parsed = response.json().unwrap_or(Value::Null);
        // A 2xx that still carries a non-zero `code` is a rejection, not a delivery.
        let rejected = body_code(&parsed).is_some_and(|code| code != 0);
        if !response.ok() || rejected {
            let failure = classify(response.status, &parsed, response.retry_after);
            if failure.token_invalid {
                self.invalidate_token();
            }
            return Err(failure);
        }
        match parsed.get("id").and_then(value_to_id) {
            Some(message_id) => Ok(message_id),
            None => Err(Failure::unknown(
                "qqbot send response carried no message id",
            )),
        }
    }

    /// `POST .../files` for a URL source — hermes `_upload_media(..., url=…, srv_send_msg=False)` —
    /// returning the 2xx body the RichMedia message needs (`file_info` lives in there).
    ///
    /// QQ fetches the URL itself, so no bytes travel from here. The path is
    /// [`rest_path`]`(kind, chat_id, "files")`, the same `/v2/{groups|users}/{id}` shape as the
    /// message endpoints: guild channels have no such path in the reference (its `_rest_path`
    /// knows only `c2c` and `group`), and [`PlatformAdapter::send_media`] refuses them before this
    /// point.
    ///
    /// No retry loop here on purpose: hermes wraps the upload in three attempts and sleeps between
    /// them (`1.5 * (attempt + 1)`). This adapter returns the typed failure — `transient` with
    /// QQ's `retry_after` when it sent one — and lets the delivery ledger own the wait instead of
    /// blocking a send task.
    async fn upload_media_url(
        &self,
        kind: ChatKind,
        chat_id: &str,
        file_type: i64,
        url: &str,
        file_name: Option<&str>,
    ) -> Result<Value, Failure> {
        let Some(path) = rest_path(kind, chat_id, "files") else {
            return Err(Failure::unknown(
                "qqbot has no media upload path for a guild channel",
            ));
        };
        let token = self.acquire_token().await?;
        let request = HttpRequest::json(
            format!("{}{}", self.api_base, path),
            &upload_body(file_type, url, file_name),
        )
        .header("Authorization", format!("QQBot {token}"));
        let response = self.transport.execute(request).await.map_err(|err| {
            Failure::transient(format!("qqbot media upload transport error: {err}"))
        })?;
        let parsed = response.json().unwrap_or(Value::Null);
        // Same rule as a text chunk: a 2xx that still carries a non-zero `code` is a rejection.
        let rejected = body_code(&parsed).is_some_and(|code| code != 0);
        if !response.ok() || rejected {
            let failure = classify(response.status, &parsed, response.retry_after);
            if failure.token_invalid {
                self.invalidate_token();
            }
            return Err(failure);
        }
        Ok(parsed)
    }

    /// `POST .../messages` carrying the RichMedia body, returning the delivery result
    /// (hermes `_post_message` on `_send_media`'s body). Like the text path it reports the `id`
    /// QQ assigned, keeps the whole response in `raw_response`, and drops the cached token when
    /// the body blames it.
    async fn post_media_message(
        &self,
        kind: ChatKind,
        chat_id: &str,
        file_info: &Value,
        caption: Option<&str>,
        reply_to: Option<&str>,
    ) -> Result<SendResult, Failure> {
        let token = self.acquire_token().await?;
        let url = format!("{}{}", self.api_base, messages_path(kind, chat_id));
        let request = HttpRequest::json(url, &media_body(file_info, caption, reply_to))
            .header("Authorization", format!("QQBot {token}"));
        let response = self.transport.execute(request).await.map_err(|err| {
            Failure::transient(format!("qqbot media send transport error: {err}"))
        })?;
        let parsed = response.json().unwrap_or(Value::Null);
        let rejected = body_code(&parsed).is_some_and(|code| code != 0);
        if !response.ok() || rejected {
            let failure = classify(response.status, &parsed, response.retry_after);
            if failure.token_invalid {
                self.invalidate_token();
            }
            return Err(failure);
        }
        let Some(message_id) = parsed.get("id").and_then(value_to_id) else {
            return Err(Failure::unknown(
                "qqbot media send response carried no message id",
            ));
        };
        let mut result = SendResult::ok(message_id);
        result.raw_response = Some(parsed);
        Ok(result)
    }
}

#[async_trait]
impl PlatformAdapter for QqBotAdapter {
    /// Fetch an app access token. True only when [`QQ_TOKEN_URL`] answers with an `access_token`,
    /// which is the only proof the credentials are live. There is no long-lived connection to
    /// re-establish, so `is_reconnect` needs no different handling.
    async fn connect(&mut self, _is_reconnect: bool) -> bool {
        self.acquire_token().await.is_ok()
    }

    /// Forget the cached token. Nothing else is held client-side: the host owns the WebSocket
    /// gateway, so there is no session to tear down.
    async fn disconnect(&mut self) {
        self.invalidate_token();
    }

    /// Trim, chunk to [`QQ_MAX_MESSAGE_LENGTH`] code points and deliver the chunks in order.
    ///
    /// The endpoint comes from `metadata["chat_type"]` (`"guild"` | `"group"` | `"dm"`) when the
    /// caller supplies it — [`QqBotAdapter::parse_webhook`] sets it on every inbound event, so a
    /// reply routes back to the surface the message arrived on — and is otherwise inferred from
    /// the id shape ([`infer_chat_kind`]).
    ///
    /// Only the opening chunk carries the `msg_id` passive-reply anchor (hermes clears `reply_to`
    /// after the first chunk): repeating it would make QQ treat the tail chunks as replies to the
    /// same passive message, which it rate-limits.
    ///
    /// Blank content is refused with `bad_format` before any request: an empty text message is not
    /// something QQ can deliver, and silently succeeding would hide a broken reply.
    async fn send(
        &self,
        chat_id: &str,
        content: &str,
        reply_to: Option<&str>,
        metadata: Option<Value>,
    ) -> SendResult {
        let trimmed = content.trim();
        if trimmed.is_empty() {
            return SendResult::failed(
                "qqbot cannot send an empty text message",
                Some("bad_format"),
            );
        }
        let kind = chat_kind(chat_id, metadata.as_ref());
        let chunks = truncate_message(trimmed, QQ_MAX_MESSAGE_LENGTH, codepoint_len);
        if chunks.is_empty() {
            return SendResult::failed(
                "qqbot cannot send an empty text message",
                Some("bad_format"),
            );
        }
        let mut delivered: Vec<String> = Vec::with_capacity(chunks.len());
        for (index, chunk) in chunks.iter().enumerate() {
            let anchor = if index == 0 { reply_to } else { None };
            match self.send_chunk(kind, chat_id, chunk, anchor).await {
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
            None => SendResult::failed("qqbot delivered no chunks", Some("unknown")),
        }
    }

    /// No lookup exists: QQ's OpenAPI has no chat-info endpoint, so this reports the documented
    /// fallback shape `{name: chat_id, chat_type: "group"}` without a request (hermes
    /// `get_chat_info` returns the same id-as-name pair from its in-memory chat-type map).
    async fn get_chat_info(&self, chat_id: &str) -> ChatInfo {
        ChatInfo {
            name: chat_id.to_string(),
            chat_type: "group".to_string(),
        }
    }

    /// True: [`Self::send_media`] implements QQ's RichMedia flow for URL sources.
    fn supports_media(&self) -> bool {
        true
    }

    /// Send one attachment through QQ's RichMedia flow (hermes `_send_media`).
    ///
    /// A [`MediaSource::Url`] is fetched by QQ itself, so the wire flow is exactly two requests:
    ///
    /// 1. `POST {api_base}/v2/{groups|users}/{chat_id}/files` with
    ///    `{"file_type": …, "srv_send_msg": false, "url": …, "file_name"?: …}`
    ///    ([`upload_body`]): `file_type` comes from [`file_type_for`] (`1` image, `2` video,
    ///    `3` voice, `4` document), `srv_send_msg: false` because step 2 sends the message itself,
    ///    and `file_name` is added for documents only — hermes passes it
    ///    (`resolved_name if file_type == MEDIA_TYPE_FILE else None`).
    /// 2. `POST .../messages` on the same surface as text, with `{"msg_type": 7,
    ///    "media": {"file_info": …}, "msg_seq": …, "content"?: caption, "msg_id"?: reply_to}`
    ///    ([`media_body`]). `file_info` is lifted out of the upload response — top-level or under
    ///    `data` — and a response without one is a typed failure, never a silent success.
    ///
    /// Attachments refused **before any request**, each with the reason the reference gives:
    ///
    /// - a guild channel: hermes' `_send_media` answers `"Guild media send not supported via this
    ///   path"` before looking at the source, because guild media is a different API;
    /// - a [`MediaSource::Bytes`] attachment: QQ turns local bytes into a `file_info` only through
    ///   the chunked uploader that is not ported here, so the refusal names
    ///   [`MediaSource::Url`] instead of inventing an endpoint (see [`bytes_media_refusal`]);
    /// - a blank URL: QQ would fetch nothing and answer with an opaque 400;
    /// - a URL that is not `http(s)`: QQ's `/files` `url=` exchange only fetches `http`/`https`,
    ///   and the reference would read any other spelling as a *local path* and send it through the
    ///   chunked uploader (see the previous point).
    ///
    /// Failures are classified by [`classify`], the same taxonomy as text: `429` / 11244 / 11245 →
    /// `rate_limited` carrying `retry_after`, `401`/`403` / 11241 / 11242 → `forbidden` (dropping
    /// the token cache on 11241), `404` → `not_found`, other 4xx — `413` included, since resending
    /// the same file cannot help — → `bad_format`, 5xx and transport errors → `transient`.
    ///
    /// Unlike [`Self::send`] there is no `metadata` to carry `"chat_type"`, so the surface comes
    /// from the id shape alone ([`infer_chat_kind`]): a `channel_`/`guild_` id is a guild channel
    /// and gets the guild refusal.
    async fn send_media(
        &self,
        chat_id: &str,
        media: &OutboundMedia,
        reply_to: Option<&str>,
    ) -> SendResult {
        let kind = infer_chat_kind(chat_id);
        // hermes checks the guild branch before it looks at the source, so this refusal wins.
        if kind == ChatKind::Guild {
            return SendResult::failed(
                "Guild media send not supported via this path",
                Some("bad_format"),
            );
        }
        let url = match &media.source {
            MediaSource::Url(url) => url.trim(),
            MediaSource::Bytes(bytes) => return bytes_media_refusal(bytes.len()),
        };
        if url.is_empty() {
            return SendResult::failed(
                "qqbot cannot send media from an empty media url",
                Some("bad_format"),
            );
        }
        if !is_http_url(url) {
            return SendResult::failed(
                format!(
                    "qqbot only fetches http(s) media urls; refusing {url:?}: the reference reads a \
                     non-url media source as a local path and sends it through the chunked uploader, \
                     which this adapter does not port — pass an http(s) MediaSource::Url"
                ),
                Some("bad_format"),
            );
        }
        let file_type = file_type_for(media.kind);
        // Only a document travels with a name; the reference drops the name for every other type.
        let file_name = if file_type == MEDIA_TYPE_FILE {
            Some(document_file_name(media, url))
        } else {
            None
        };
        let caption = media
            .caption
            .as_deref()
            .filter(|caption| !caption.trim().is_empty())
            .map(|caption| cap_codepoints(caption, QQ_MAX_MESSAGE_LENGTH));

        let upload = match self
            .upload_media_url(kind, chat_id, file_type, url, file_name.as_deref())
            .await
        {
            Ok(upload) => upload,
            Err(failure) => return failure.into_send_result(),
        };
        let Some(file_info) = file_info_of(&upload) else {
            return Failure::unknown(
                "qqbot media upload returned no file_info; QQ cannot address the uploaded media",
            )
            .into_send_result();
        };
        match self
            .post_media_message(kind, chat_id, &file_info, caption.as_deref(), reply_to)
            .await
        {
            Ok(result) => result,
            Err(failure) => failure.into_send_result(),
        }
    }

    /// QQ renders fenced code blocks in markdown-capable bot messages (hermes
    /// `markdown_support`, default on).
    fn supports_code_blocks(&self) -> bool {
        true
    }

    /// `send` chunks natively; the delivery router must not truncate first.
    fn splits_long_messages(&self) -> bool {
        true
    }

    /// The per-message cap `send` chunks against, so the router and the sender agree.
    fn max_message_length(&self) -> usize {
        QQ_MAX_MESSAGE_LENGTH
    }

    /// QQ counts code points, which is already the trait default; stated explicitly so the cap and
    /// the chunker agree on the unit.
    fn message_length(&self, text: &str) -> usize {
        codepoint_len(text)
    }

    /// No-op: QQ's OpenAPI has no typing-indicator endpoint. The C2C `input_notify` frame
    /// (`msg_type: 6`) is a *passive reply* that needs the last inbound message id and a 50 s
    /// debounce table — per-chat state the runner owns, not a stateless wire adapter. A typing
    /// hint is cosmetic, so skipping it is preferable to inventing a request.
    async fn send_typing(&self, _chat_id: &str, _metadata: Option<Value>) {}

    /// Honest refusal: QQ's OpenAPI has no endpoint that edits an already-sent bot message, so the
    /// adapter says so instead of pretending. Callers fall back to a fresh `send`; the streaming
    /// delivery path treats `bad_format` as "edit unsupported, send instead".
    async fn edit_message(
        &self,
        _chat_id: &str,
        _message_id: &str,
        _content: &str,
        _finalize: bool,
    ) -> SendResult {
        SendResult::failed(
            "qqbot does not support editing sent messages; send a new one",
            Some("bad_format"),
        )
    }

    /// No-op: QQ's OpenAPI has no endpoint that deletes a bot message, so there is nothing to
    /// call and nothing to remember client-side.
    async fn delete_message(&self, _chat_id: &str, _message_id: &str) {}
}

/// How one QQ Open Platform call failed, carrying everything the send taxonomy needs.
#[derive(Debug)]
struct Failure {
    message: String,
    kind: &'static str,
    retryable: bool,
    retry_after: Option<f64>,
    /// The body blamed the token, so the cache entry is worthless.
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

    /// The shared [`SendResult`] shape: `error_kind` always set, `retryable`/`retry_after` only
    /// when the failure is worth another attempt.
    fn into_send_result(self) -> SendResult {
        let mut result = SendResult::failed(self.message, Some(self.kind));
        if self.retryable {
            result = result.retryable(self.retry_after);
        }
        result
    }
}

/// QQ's failure taxonomy: the body's `code` first (it is more specific than the status), then the
/// HTTP status.
///
/// - 11244 / 11245 or HTTP 429 (frequency limit) → `rate_limited`, retryable, honouring
///   `Retry-After`;
/// - 11241 / 11242 or HTTP 401 / 403 (token rejected) → `forbidden`, non-retryable;
/// - HTTP 404 (unknown chat or message) → `not_found`, non-retryable;
/// - other 4xx → `bad_format`, non-retryable;
/// - 5xx → `transient`, retryable;
/// - a 2xx that still carried a non-zero `code` → `bad_format` (the payload was refused, so
///   retrying the same text cannot help);
/// - anything else (1xx / 3xx) → `transient`, retryable: nothing was delivered.
fn classify(status: u16, body: &Value, retry_after: Option<f64>) -> Failure {
    let code = body_code(body);
    let message = render_error(status, code, body);
    let token_invalid = code == Some(TOKEN_INVALID_CODE);
    let kind = if status == 429 || code.is_some_and(|code| RATE_LIMIT_CODES.contains(&code)) {
        "rate_limited"
    } else if matches!(status, 401 | 403)
        || code.is_some_and(|code| TOKEN_REJECTED_CODES.contains(&code))
    {
        "forbidden"
    } else if status == 404 {
        "not_found"
    } else {
        match status {
            500..=599 => "transient",
            400..=499 => "bad_format",
            _ if code.is_some_and(|code| code != 0) => "bad_format",
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
fn credentials_failure(status: u16, body: &Value, retry_after: Option<f64>) -> Failure {
    let mut failure = classify(status, body, retry_after);
    if matches!(status, 400 | 401 | 403) {
        failure.kind = "forbidden";
        failure.retryable = false;
        failure.retry_after = None;
        failure.token_invalid = false;
    }
    failure
}

/// Which surface a chat id belongs to, preferring the caller's `metadata["chat_type"]` hint (the
/// value [`QqBotAdapter::parse_webhook`] stamps onto inbound events) over id inference.
fn chat_kind(chat_id: &str, metadata: Option<&Value>) -> ChatKind {
    let hinted = metadata
        .and_then(|metadata| metadata.get("chat_type"))
        .and_then(ChatKind::from_hint);
    hinted.unwrap_or_else(|| infer_chat_kind(chat_id))
}

/// Guess the surface from the id shape, the way hermes' `_guess_chat_type` guesses from its
/// inbound map (which a stateless adapter does not have).
///
/// - a `group_` prefix, or a purely numeric id (QQ group openids arrive in numeric form in several
///   payloads) → group;
/// - a `channel_` / `guild_` prefix (guild channel ids are snowflakes the platform itself
///   serializes with that prefix when it hands them to a bot) → guild;
/// - a `user_` / `friend_` / `openid_` prefix, or any other opaque openid → C2C.
fn infer_chat_kind(chat_id: &str) -> ChatKind {
    let trimmed = chat_id.trim();
    let lowered = trimmed.to_ascii_lowercase();
    if lowered.starts_with("group_") {
        return ChatKind::Group;
    }
    if lowered.starts_with("channel_") || lowered.starts_with("guild_") {
        return ChatKind::Guild;
    }
    if lowered.starts_with("user_")
        || lowered.starts_with("friend_")
        || lowered.starts_with("openid_")
    {
        return ChatKind::Dm;
    }
    if !trimmed.is_empty() && trimmed.bytes().all(|byte| byte.is_ascii_digit()) {
        return ChatKind::Group;
    }
    ChatKind::Dm
}

/// The request path for one chunk (`_messages_path` / `_rest_path` in hermes).
fn messages_path(kind: ChatKind, chat_id: &str) -> String {
    let id = encode_path_segment(chat_id.trim());
    match kind {
        ChatKind::Guild => format!("/channels/{id}/messages"),
        ChatKind::Group => format!("/v2/groups/{id}/messages"),
        ChatKind::Dm => format!("/v2/users/{id}/messages"),
    }
}

/// The outbound body: `{"content", "msg_type": 0}`, plus the passive-reply `msg_id`/`msg_seq`
/// pair on the group and C2C endpoints (hermes `_build_text_body`, `_send_text_to`).
fn text_body(kind: ChatKind, chunk: &str, reply_to: Option<&str>) -> Value {
    let mut body = json!({ "content": chunk, "msg_type": MSG_TYPE_TEXT });
    if matches!(kind, ChatKind::Group | ChatKind::Dm)
        && let Some(reply_to) = reply_to.map(str::trim).filter(|anchor| !anchor.is_empty())
    {
        body["msg_id"] = json!(reply_to);
        // A fixed sequence of 1: this adapter sends at most one passive reply per turn.
        body["msg_seq"] = json!(1);
    }
    body
}

/// The `/files` path for one media surface — hermes `_rest_path`:
/// `/v2/{groups|users}/{id}/{endpoint}`, `users` for a C2C openid and `groups` otherwise.
///
/// `None` for a guild channel: the reference's `_rest_path` has no guild arm at all (guild media is
/// a different API), so there is no path to build, let alone invent.
fn rest_path(kind: ChatKind, chat_id: &str, endpoint: &str) -> Option<String> {
    let collection = match kind {
        ChatKind::Guild => return None,
        ChatKind::Group => "groups",
        ChatKind::Dm => "users",
    };
    let id = encode_path_segment(chat_id.trim());
    Some(format!("/v2/{collection}/{id}/{endpoint}"))
}

/// QQ's upload `file_type` for an outbound [`MediaKind`] (hermes `MEDIA_TYPE_IMAGE` / `_VIDEO` /
/// `_VOICE` / `_FILE`, as `send_image` / `send_video` / `send_voice` / `send_document` pass them).
///
/// [`MediaKind::Audio`] maps to QQ's *voice* type: QQ has one audio upload class and the reference
/// has no separate music/audio-file type.
fn file_type_for(kind: MediaKind) -> i64 {
    match kind {
        MediaKind::Image => MEDIA_TYPE_IMAGE,
        MediaKind::Video => MEDIA_TYPE_VIDEO,
        MediaKind::Audio => MEDIA_TYPE_VOICE,
        MediaKind::Document => MEDIA_TYPE_FILE,
    }
}

/// The `/files` upload body (hermes `_upload_media`).
///
/// `srv_send_msg` is always `false`: the media message is the second step, and letting QQ post it
/// itself would deliver an attachment with no caption, no `msg_id` anchor and no message id this
/// adapter could report. `file_name` is only ever passed for [`MEDIA_TYPE_FILE`] — the reference
/// interpolates it under exactly that condition.
fn upload_body(file_type: i64, url: &str, file_name: Option<&str>) -> Value {
    let mut body = json!({
        "file_type": file_type,
        "srv_send_msg": false,
        "url": url,
    });
    if let Some(file_name) = file_name {
        body["file_name"] = json!(file_name);
    }
    body
}

/// The RichMedia message body (hermes `_send_media`'s `body`):
/// `{"msg_type": 7, "media": {"file_info": …}, "msg_seq": …}`, plus the capped caption as
/// `content` and the caller's `msg_id` anchor when either is present.
fn media_body(file_info: &Value, caption: Option<&str>, reply_to: Option<&str>) -> Value {
    let mut body = json!({
        "msg_type": MSG_TYPE_MEDIA,
        "media": { "file_info": file_info },
        "msg_seq": PASSIVE_MSG_SEQ,
    });
    if let Some(caption) = caption {
        body["content"] = json!(caption);
    }
    if let Some(reply_to) = reply_to.map(str::trim).filter(|anchor| !anchor.is_empty()) {
        body["msg_id"] = json!(reply_to);
    }
    body
}

/// The `file_info` an upload hands back for the RichMedia message: top-level, else under `data` —
/// hermes `upload.get("file_info") or (upload.get("data") or {}).get("file_info")`.
///
/// An absent, `null`, blank-string or empty-object/array value counts as "no file_info" (Python's
/// falsy test has the same effect): QQ has not registered the media, and a media message without a
/// usable `file_info` would fail with an opaque platform error instead of a diagnosable one.
fn file_info_of(upload: &Value) -> Option<Value> {
    let candidate = upload
        .get("file_info")
        .or_else(|| upload.get("data")?.get("file_info"))?;
    let usable = match candidate {
        Value::Null => false,
        Value::String(text) => !text.trim().is_empty(),
        Value::Object(fields) => !fields.is_empty(),
        Value::Array(items) => !items.is_empty(),
        Value::Bool(present) => *present,
        Value::Number(_) => true,
    };
    if usable {
        Some(candidate.clone())
    } else {
        None
    }
}

/// The `file_name` a document upload carries — hermes
/// `file_name or Path(urlparse(media_source).path).name or "media"`:
/// the caller's name, else the URL's last path segment, else the literal `media`.
fn document_file_name(media: &OutboundMedia, url: &str) -> String {
    media
        .filename
        .as_deref()
        .map(str::trim)
        .filter(|name| !name.is_empty())
        .map(str::to_string)
        .or_else(|| url_last_path_segment(url))
        .unwrap_or_else(|| "media".to_string())
}

/// The last segment of a URL's path (`Path(urlparse(url).path).name`), without a URL parser: the
/// authority is skipped, and the query/fragment tail is not part of the name.
fn url_last_path_segment(url: &str) -> Option<String> {
    let trimmed = url.trim();
    let after_scheme = match trimmed.find("://") {
        Some(separator) => &trimmed[separator + "://".len()..],
        None => trimmed,
    };
    // `https://host` carries no path, so it carries no file name either.
    let start = after_scheme.find('/')?;
    let path = &after_scheme[start..];
    let path = path.split(['?', '#']).next().unwrap_or(path);
    let segment = path.rsplit('/').next().unwrap_or_default().trim();
    if segment.is_empty() {
        None
    } else {
        Some(segment.to_string())
    }
}

/// hermes `_is_url`: the two schemes QQ's `/files` `url=` exchange will fetch itself.
/// `urlparse` lowercases the scheme, so the test does too.
fn is_http_url(value: &str) -> bool {
    let lowered = value.trim().to_ascii_lowercase();
    lowered.starts_with("http://") || lowered.starts_with("https://")
}

/// hermes `caption[: MAX_MESSAGE_LENGTH]`: a caption is capped, never chunked, and the cap counts
/// code points — slicing at a byte offset would panic on a multi-byte boundary.
fn cap_codepoints(text: &str, max_len: usize) -> String {
    if text.chars().count() <= max_len {
        return text.to_string();
    }
    text.chars().take(max_len).collect()
}

/// Refuse a [`MediaSource::Bytes`] attachment without touching the network.
///
/// QQ has no single-request byte upload: local bytes become a `file_info` only through
/// `chunked_upload.py`'s three-stage flow — `POST .../upload_prepare` (which returns
/// `upload_id`, `block_size` and pre-signed COS part URLs), a `PUT` of each part plus
/// `POST .../upload_part_finish`, then `POST .../files` with the `upload_id`. That uploader also
/// owns per-part retries (`40093001`), the `40093002` daily-quota error, the `md5_10m` hash and
/// the part-concurrency policy. None of it is ported here, so the honest answer is a refusal that
/// names the supported source — [`MediaSource::Url`], which needs no uploader because QQ fetches
/// the media itself. Zero requests are made and no success is reported.
fn bytes_media_refusal(byte_len: usize) -> SendResult {
    SendResult::failed(
        format!(
            "qqbot cannot upload {byte_len} bytes of MediaSource::Bytes media: QQ turns local bytes \
             into a file_info only through its chunked uploader (POST .../upload_prepare, a \
             pre-signed COS PUT per block, POST .../upload_part_finish, then POST .../files with the \
             upload_id), and that uploader (chunked_upload.py: quota codes 40093002/40093001, \
             md5_10m, per-part retries) is not ported to this adapter — pass MediaSource::Url so QQ \
             fetches the media itself"
        ),
        Some("bad_format"),
    )
}

/// QQ's `code` field, when the body carries one (as a number or a numeric string).
fn body_code(body: &Value) -> Option<i64> {
    value_to_i64(body.get("code"))
}

/// Human-readable error text: `[code] message` when QQ supplied a code, else the HTTP status.
fn render_error(status: u16, code: Option<i64>, body: &Value) -> String {
    let message = ["message", "msg"]
        .iter()
        .find_map(|key| body.get(*key).and_then(Value::as_str))
        .map(str::trim)
        .filter(|message| !message.is_empty());
    match (code, message) {
        (Some(code), Some(message)) => format!("qqbot error {code} (HTTP {status}): {message}"),
        (Some(code), None) => format!("qqbot error {code} (HTTP {status})"),
        (None, Some(message)) => format!("qqbot HTTP {status}: {message}"),
        (None, None) => format!("qqbot HTTP {status}"),
    }
}

/// A JSON value as an `i64`, accepting the numeric-string form QQ uses for `expires_in`.
fn value_to_i64(value: Option<&Value>) -> Option<i64> {
    match value? {
        Value::Number(number) => number.as_i64(),
        Value::String(text) => text.trim().parse::<i64>().ok(),
        _ => None,
    }
}

/// A message id as a string: QQ sends ids as strings, but mocked or legacy payloads may use
/// numbers.
fn value_to_id(value: &Value) -> Option<String> {
    match value {
        Value::Number(number) => number
            .as_i64()
            .map(|id| id.to_string())
            .or_else(|| number.as_u64().map(|id| id.to_string())),
        Value::String(text) => {
            let trimmed = text.trim();
            if trimmed.is_empty() {
                None
            } else {
                Some(trimmed.to_string())
            }
        }
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

/// The gateway's platform handle for this adapter. The constant is a fixed non-empty name, so the
/// fallback arm is unreachable and exists only to keep this function panic-free.
fn qqbot_platform() -> Platform {
    match Platform::new(platform_names::QQBOT) {
        Some(platform) => platform,
        None => Platform::local(),
    }
}

/// Percent-encode one URL path segment (unreserved characters kept, everything else `%XX`) so a
/// chat id can never re-shape the request path.
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

/// The Ed25519 public key QQ derives from a webhook `seed`, or `None` if ring refuses the seed.
///
/// `h = SHA-512(seed)`, clamp `h[0..32]` (`h[0] &= 248`, `h[31] &= 127`, `h[31] |= 64`),
/// `a = h[0..32] mod L`, `A = [a]B` — RFC 8032 §5.1.5. `from_seed_unchecked` performs exactly that
/// derivation (seed expansion, clamping and the base-point multiplication) inside ring; the test
/// module asserts the resulting bytes against the formula spelled out over `curve25519-dalek`, and
/// those bytes are what [`QqBotAdapter::verify_webhook`] hands to the verifier as `A`.
fn webhook_public_key(seed: &[u8; ED25519_SEED_LEN]) -> Option<[u8; ED25519_HALF_LEN]> {
    let key_pair = Ed25519KeyPair::from_seed_unchecked(seed).ok()?;
    <[u8; ED25519_HALF_LEN]>::try_from(key_pair.public_key().as_ref()).ok()
}

/// Whether the 32-byte little-endian scalar `bytes` is canonical, i.e. strictly below `L`.
///
/// One pass with no data-dependent branching: subtract `L` and read the final borrow, which is set
/// exactly when `bytes < L`. Comparing the little-endian form directly is not enough — the bound is
/// `2^252 + …`, not a bit length — so the subtraction is what decides it.
fn scalar_is_canonical(bytes: &[u8; ED25519_HALF_LEN]) -> bool {
    let mut borrow = 0i16;
    for (byte, order) in bytes.iter().zip(ED25519_ORDER_L.iter()) {
        borrow = i16::from(*byte as i16 - *order as i16 - borrow < 0);
    }
    borrow == 1
}

/// Hex-decode `value`, accepting either case, rejecting an odd length or a non-hex character.
fn decode_hex_bytes(value: &str) -> Option<Vec<u8>> {
    let (pairs, remainder) = value.as_bytes().as_chunks::<2>();
    if !remainder.is_empty() {
        // An odd number of hex digits cannot be a byte string.
        return None;
    }
    let mut out = Vec::with_capacity(pairs.len());
    for pair in pairs {
        out.push(hex_byte(pair[0])? << 4 | hex_byte(pair[1])?);
    }
    Some(out)
}

/// One hex digit as its `0..=15` value.
fn hex_byte(byte: u8) -> Option<u8> {
    match byte {
        b'0'..=b'9' => Some(byte - b'0'),
        b'a'..=b'f' => Some(byte - b'a' + 0x0a),
        b'A'..=b'F' => Some(byte - b'A' + 0x0a),
        _ => None,
    }
}

/// Strip one leading mention marker (`<@!1234>` or `<@1234>`) and the whitespace after it.
///
/// hermes `_strip_at_mention` does the same with `^@\S+\s*` on group messages; guild and C2C
/// payloads carry the Discord-style `<@…>` form, so both spellings are removed here. Content with
/// no marker is returned with only its leading whitespace trimmed.
fn strip_leading_mention(content: &str) -> &str {
    let trimmed = content.trim_start();
    if !trimmed.starts_with("<@") {
        return trimmed;
    }
    match trimmed.find('>') {
        Some(end) => trimmed[end + 1..].trim_start(),
        // An unterminated marker is not a mention; keep the text as it arrived.
        None => trimmed,
    }
}

/// Build the normalized [`MessageEvent`] from one `op: 0` dispatch payload.
///
/// Returns `None` when the frame carries nothing this adapter can represent (blank content after
/// mention stripping, or no chat id at all) so [`QqBotAdapter::parse_webhook`] skips it instead of
/// faking an event.
fn parse_dispatch(kind: ChatKind, data: &Value) -> Option<MessageEvent> {
    let text = strip_leading_mention(
        data.get("content")
            .and_then(Value::as_str)
            .unwrap_or_default(),
    );
    if text.trim().is_empty() {
        return None;
    }
    let author = data.get("author").filter(|author| author.is_object());
    // Guild authors carry `id`; group members `member_openid`; C2C users `user_openid`. A frame
    // with no attributable author is not a message this adapter can represent.
    let user_id = text_field(author, "id")
        .or_else(|| text_field(author, "member_openid"))
        .or_else(|| text_field(author, "user_openid"))?;
    let user_name = text_field(author, "username");
    // Guild → `channel_id`, group → `group_openid`, C2C → the author's own id, in that order.
    let chat_id = text_field(Some(data), "channel_id")
        .or_else(|| text_field(Some(data), "group_openid"))
        .or_else(|| Some(user_id.clone()))?;
    let message_id = text_field(Some(data), "id");
    // QQ timestamps are epoch milliseconds in older frames and ISO-8601 strings in current ones;
    // the gateway field is a numeric update id, so an ISO-8601 stamp leaves it unset.
    let update_id = value_to_i64(data.get("timestamp"));

    let mut source = SessionSource::new(qqbot_platform(), chat_id.clone());
    source.chat_type = kind.as_str().to_string();
    source.user_id = Some(user_id.clone());
    source.user_name = user_name.clone();
    source.message_id = message_id.clone();

    let mut event = MessageEvent::new(text.to_string(), source);
    event.user_id = Some(user_id);
    event.user_name = user_name;
    event.message_id = message_id;
    event.platform_update_id = update_id;
    event.raw_message = Some(data.clone());
    // The surface this message arrived on travels with it, so a reply's `send` picks the same
    // endpoint without re-inferring it from the id.
    event
        .metadata
        .insert("chat_type".to_string(), json!(kind.as_str()));
    Some(event)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::transport::{HttpMethod, HttpResponse, Transport, TransportError};
    use curve25519_dalek::edwards::{CompressedEdwardsY, EdwardsPoint};
    use curve25519_dalek::scalar::Scalar;
    use ring::digest::{self, SHA512};
    use std::collections::VecDeque;
    use std::sync::{Arc, Mutex, MutexGuard};

    /// Records every request and replays scripted responses in order.
    struct MockTransport {
        requests: Mutex<Vec<HttpRequest>>,
        responses: Mutex<VecDeque<Result<HttpResponse, TransportError>>>,
    }

    impl MockTransport {
        fn new(responses: Vec<Result<HttpResponse, TransportError>>) -> Arc<Self> {
            Arc::new(Self {
                requests: Mutex::new(Vec::new()),
                responses: Mutex::new(responses.into()),
            })
        }

        fn requests(&self) -> Vec<HttpRequest> {
            lock(&self.requests).clone()
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
    fn lock<T>(guard: &Mutex<T>) -> MutexGuard<'_, T> {
        guard
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
    }

    fn request_body(request: &HttpRequest) -> Value {
        let bytes = request.body.clone().unwrap_or_default();
        serde_json::from_slice(&bytes).unwrap_or(Value::Null)
    }

    fn response(status: u16, body: Value) -> Result<HttpResponse, TransportError> {
        Ok(HttpResponse {
            status,
            body: body.to_string(),
            retry_after: None,
        })
    }

    fn token_ok(token: &str) -> Result<HttpResponse, TransportError> {
        // QQ returns `expires_in` as a string, which the adapter must accept.
        response(200, json!({ "access_token": token, "expires_in": "7200" }))
    }

    fn message_sent(message_id: &str) -> Result<HttpResponse, TransportError> {
        response(
            200,
            json!({ "id": message_id, "timestamp": "2024-01-01T00:00:00Z" }),
        )
    }

    fn chat_error(status: u16, code: i64, message: &str) -> Result<HttpResponse, TransportError> {
        response(status, json!({ "code": code, "message": message }))
    }

    fn adapter(mock: &Arc<MockTransport>) -> QqBotAdapter {
        QqBotAdapter::new(mock.clone(), "APP-ID", "CLIENT-SECRET")
    }

    fn header<'a>(request: &'a HttpRequest, name: &str) -> Option<&'a str> {
        request
            .headers
            .iter()
            .find(|(key, _)| key.eq_ignore_ascii_case(name))
            .map(|(_, value)| value.as_str())
    }

    fn non_whitespace(text: &str) -> String {
        text.chars().filter(|ch| !ch.is_whitespace()).collect()
    }

    fn content_of(request: &HttpRequest) -> String {
        request_body(request)
            .get("content")
            .and_then(Value::as_str)
            .unwrap_or_default()
            .to_string()
    }

    #[tokio::test]
    async fn connect_fetches_the_token_once_and_reuses_the_cache() {
        let mock = MockTransport::new(vec![
            token_ok("TOKEN-A"),
            message_sent("m-1"),
            message_sent("m-2"),
        ]);
        let mut adapter = adapter(&mock);
        assert!(adapter.connect(false).await);

        assert!(adapter.send("channel_7", "first", None, None).await.success);
        assert!(
            adapter
                .send("channel_7", "second", None, None)
                .await
                .success
        );

        let requests = mock.requests();
        // One token fetch for both sends: the cache is reused.
        assert_eq!(requests.len(), 3);
        assert_eq!(requests[0].method, HttpMethod::Post);
        assert_eq!(requests[0].url, QQ_TOKEN_URL);
        let token_body = request_body(&requests[0]);
        assert_eq!(token_body["appId"], json!("APP-ID"));
        assert_eq!(token_body["clientSecret"], json!("CLIENT-SECRET"));
        // The token endpoint is not the message API and needs no bearer header.
        assert!(header(&requests[0], "Authorization").is_none());
        for request in &requests[1..] {
            assert_eq!(header(request, "Authorization"), Some("QQBot TOKEN-A"));
        }
        // `access_token` reports the cached value without a second round trip.
        assert_eq!(adapter.access_token().await.as_deref(), Ok("TOKEN-A"));
        assert_eq!(mock.requests().len(), 3);
    }

    #[tokio::test]
    async fn connect_is_false_when_the_token_request_fails() {
        // A refused credential pair.
        let refused = MockTransport::new(vec![chat_error(401, 100_016, "invalid appid")]);
        assert!(!adapter(&refused).connect(false).await);
        let failure = adapter(&refused).access_token().await;
        assert!(failure.is_err());

        // A 200 with no access_token.
        let empty = MockTransport::new(vec![response(200, json!({ "expires_in": "7200" }))]);
        assert!(!adapter(&empty).connect(false).await);

        // A transport failure.
        let down = MockTransport::new(vec![Err(TransportError::Request("dns".to_string()))]);
        assert!(!adapter(&down).connect(false).await);

        // A body that is not JSON at all.
        let garbage = MockTransport::new(vec![Ok(HttpResponse {
            status: 200,
            body: "<html>".to_string(),
            retry_after: None,
        })]);
        assert!(!adapter(&garbage).connect(false).await);
    }

    #[tokio::test]
    async fn guild_send_uses_the_channels_endpoint() {
        let mock = MockTransport::new(vec![token_ok("TOKEN-A"), message_sent("ROBOT1.0_abc")]);
        let adapter = adapter(&mock).with_api_base("https://api.example.test/");
        let result = adapter
            .send(
                "channel_42",
                "hello",
                Some("msg-9"),
                Some(json!({ "chat_type": "guild" })),
            )
            .await;

        assert!(result.success);
        assert_eq!(result.message_id.as_deref(), Some("ROBOT1.0_abc"));
        assert_eq!(result.continuation_message_ids, vec!["ROBOT1.0_abc"]);

        let requests = mock.requests();
        assert_eq!(requests.len(), 2);
        assert_eq!(requests[1].method, HttpMethod::Post);
        // `with_api_base` redirects the message API and drops the trailing slash.
        assert_eq!(
            requests[1].url,
            "https://api.example.test/channels/channel_42/messages"
        );
        assert_eq!(header(&requests[1], "Authorization"), Some("QQBot TOKEN-A"));
        // Guild bodies carry only content + msg_type: no passive-reply fields.
        assert_eq!(
            request_body(&requests[1]),
            json!({ "content": "hello", "msg_type": 0 })
        );
    }

    #[tokio::test]
    async fn group_send_carries_the_passive_reply_anchor() {
        let mock = MockTransport::new(vec![token_ok("TOKEN-A"), message_sent("m-7")]);
        let result = adapter(&mock)
            .send("group_abc", "pong", Some("msg-1"), None)
            .await;

        assert!(result.success);
        let requests = mock.requests();
        assert_eq!(
            requests[1].url,
            "https://api.sgroup.qq.com/v2/groups/group_abc/messages"
        );
        assert_eq!(
            request_body(&requests[1]),
            json!({ "content": "pong", "msg_type": 0, "msg_id": "msg-1", "msg_seq": 1 })
        );

        // Without an anchor the passive-reply fields are omitted entirely.
        let plain = MockTransport::new(vec![token_ok("TOKEN-A"), message_sent("m-8")]);
        assert!(
            adapter(&plain)
                .send("group_abc", "pong", None, None)
                .await
                .success
        );
        assert_eq!(
            request_body(&plain.requests()[1]),
            json!({ "content": "pong", "msg_type": 0 })
        );
    }

    #[tokio::test]
    async fn dm_send_infers_the_users_endpoint() {
        let mock = MockTransport::new(vec![token_ok("TOKEN-A"), message_sent("m-3")]);
        let result = adapter(&mock)
            .send("openid_xyz", "hi", Some("msg-2"), None)
            .await;

        assert!(result.success);
        let request = &mock.requests()[1];
        assert_eq!(
            request.url,
            "https://api.sgroup.qq.com/v2/users/openid_xyz/messages"
        );
        let body = request_body(request);
        assert_eq!(body["msg_id"], json!("msg-2"));
        assert_eq!(body["msg_seq"], json!(1));
        assert_eq!(body["msg_type"], json!(0));

        // A numeric id is a group openid; a `channel_` id is a guild channel.
        let numeric = MockTransport::new(vec![token_ok("TOKEN-A"), message_sent("m-4")]);
        assert!(
            adapter(&numeric)
                .send("1234567890", "hi", None, None)
                .await
                .success
        );
        assert_eq!(
            numeric.requests()[1].url,
            "https://api.sgroup.qq.com/v2/groups/1234567890/messages"
        );
    }

    #[tokio::test]
    async fn long_messages_are_chunked_at_the_cap() {
        let content: String = (0..140)
            .map(|idx| format!("{idx:03} a sentence that exists mostly to occupy space\n"))
            .collect();
        assert!(content.chars().count() > QQ_MAX_MESSAGE_LENGTH);

        let responses: Vec<Result<HttpResponse, TransportError>> =
            std::iter::once(token_ok("TOKEN-A"))
                .chain((0..8).map(|idx| message_sent(&format!("m-{idx}"))))
                .collect();
        let mock = MockTransport::new(responses);
        let result = adapter(&mock)
            .send("channel_5", &content, Some("msg-1"), None)
            .await;

        assert!(result.success);
        let requests = mock.requests();
        assert!(requests.len() > 2, "content should have been split");
        for request in &requests[1..] {
            assert!(
                content_of(request).chars().count() <= QQ_MAX_MESSAGE_LENGTH,
                "chunk exceeds the cap: {:?}",
                content_of(request)
            );
        }
        // Only the leading chunk replies to the anchor; the guild surface never carries it.
        assert!(
            requests[1..]
                .iter()
                .all(|request| request_body(request).get("msg_id").is_none())
        );
        // The last id is the message id and every id is reported in send order.
        let expected_last = format!("m-{}", requests.len() - 2);
        assert_eq!(result.message_id.as_deref(), Some(expected_last.as_str()));
        assert_eq!(result.continuation_message_ids.len(), requests.len() - 1);
        // Chunking loses no visible content.
        let joined: String = requests[1..].iter().map(content_of).collect();
        assert_eq!(non_whitespace(&joined), non_whitespace(content.trim()));
    }

    #[tokio::test]
    async fn rate_limited_responses_are_retryable_with_the_server_delay() {
        let throttled = MockTransport::new(vec![
            token_ok("TOKEN-A"),
            Ok(HttpResponse {
                status: 429,
                body: json!({ "code": 11244, "message": "too many requests" }).to_string(),
                retry_after: Some(12.0),
            }),
        ]);
        let result = adapter(&throttled)
            .send("channel_5", "hello", None, None)
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
                .contains("too many requests")
        );

        // The body code alone (11244 on a 400) is enough to classify it.
        let coded = MockTransport::new(vec![
            token_ok("TOKEN-A"),
            chat_error(400, 11_244, "frequency limit"),
        ]);
        let result = adapter(&coded).send("channel_5", "hello", None, None).await;
        assert_eq!(result.error_kind.as_deref(), Some("rate_limited"));
        assert!(result.retryable);
        assert_eq!(result.retry_after, None);
    }

    #[tokio::test]
    async fn token_rejection_is_forbidden_and_drops_the_cache() {
        let mock = MockTransport::new(vec![
            token_ok("TOKEN-A"),
            chat_error(401, 11_241, "token not exist or expire"),
            token_ok("TOKEN-B"),
            message_sent("m-1"),
        ]);
        let adapter = adapter(&mock);

        let refused = adapter.send("channel_5", "hello", None, None).await;
        assert!(!refused.success);
        assert_eq!(refused.error_kind.as_deref(), Some("forbidden"));
        assert!(!refused.retryable);

        // The cached token was dropped, so the next send fetches a new one before delivering.
        let delivered = adapter.send("channel_5", "hello again", None, None).await;
        assert!(delivered.success);
        assert_eq!(delivered.message_id.as_deref(), Some("m-1"));

        let requests = mock.requests();
        assert_eq!(requests.len(), 4);
        assert_eq!(requests[2].url, QQ_TOKEN_URL);
        assert_eq!(header(&requests[3], "Authorization"), Some("QQBot TOKEN-B"));
    }

    #[tokio::test]
    async fn not_found_and_server_failures_are_classified() {
        let missing = MockTransport::new(vec![
            token_ok("TOKEN-A"),
            chat_error(404, 40_034, "chat not found"),
        ]);
        let result = adapter(&missing)
            .send("channel_5", "hello", None, None)
            .await;
        assert_eq!(result.error_kind.as_deref(), Some("not_found"));
        assert!(!result.retryable);

        let server = MockTransport::new(vec![
            token_ok("TOKEN-A"),
            chat_error(502, 500_000, "internal"),
        ]);
        let result = adapter(&server)
            .send("channel_5", "hello", None, None)
            .await;
        assert_eq!(result.error_kind.as_deref(), Some("transient"));
        assert!(result.retryable);

        let down = MockTransport::new(vec![
            token_ok("TOKEN-A"),
            Err(TransportError::Request("connection reset".to_string())),
        ]);
        let result = adapter(&down).send("channel_5", "hello", None, None).await;
        assert_eq!(result.error_kind.as_deref(), Some("transient"));
        assert!(result.retryable);
    }

    #[tokio::test]
    async fn blank_content_is_refused_without_a_request() {
        let mock = MockTransport::new(Vec::new());
        let result = adapter(&mock)
            .send("channel_5", "   \n\t", None, None)
            .await;
        assert!(!result.success);
        assert_eq!(result.error_kind.as_deref(), Some("bad_format"));
        assert!(mock.requests().is_empty());
    }

    // ---- Outbound media: `POST .../files` (url=) then a RichMedia `msg_type: 7` message ----

    /// A `/files` upload response carrying `file_info` at the given shape.
    fn upload_ok(body: Value) -> Result<HttpResponse, TransportError> {
        response(200, body)
    }

    /// The upload response as QQ sends it: `file_info` at the top level.
    fn upload_with_file_info(file_info: &str) -> Result<HttpResponse, TransportError> {
        upload_ok(json!({ "file_info": file_info }))
    }

    fn url_media(kind: MediaKind, url: &str) -> OutboundMedia {
        OutboundMedia::new(kind, MediaSource::Url(url.to_string()))
    }

    #[tokio::test]
    async fn media_url_upload_body_matches_the_reference_for_each_kind() {
        // `_send_media`: only `file_type` changes with the kind; a non-document never carries
        // `file_name`, even when the caller supplies one.
        for (kind, file_type) in [
            (MediaKind::Image, MEDIA_TYPE_IMAGE),
            (MediaKind::Video, MEDIA_TYPE_VIDEO),
            (MediaKind::Audio, MEDIA_TYPE_VOICE),
        ] {
            let mock = MockTransport::new(vec![
                token_ok("TOKEN-A"),
                upload_with_file_info("FI-1"),
                message_sent("m-1"),
            ]);
            let media =
                url_media(kind, "https://cdn.example.test/pic.png").with_filename("ignored.png");
            let result = adapter(&mock)
                .send_media("group_openid_1", &media, None)
                .await;

            assert!(result.success, "{kind:?} should send");
            assert_eq!(result.message_id.as_deref(), Some("m-1"));
            assert!(result.continuation_message_ids.is_empty());

            let requests = mock.requests();
            assert_eq!(requests.len(), 3, "token + upload + media message");
            assert_eq!(requests[0].url, QQ_TOKEN_URL);
            assert_eq!(requests[1].method, HttpMethod::Post);
            assert_eq!(
                requests[1].url,
                "https://api.sgroup.qq.com/v2/groups/group_openid_1/files"
            );
            assert_eq!(header(&requests[1], "Authorization"), Some("QQBot TOKEN-A"));
            assert_eq!(
                request_body(&requests[1]),
                json!({
                    "file_type": file_type,
                    "srv_send_msg": false,
                    "url": "https://cdn.example.test/pic.png",
                }),
                "{kind:?} upload body"
            );
        }
    }

    #[tokio::test]
    async fn document_media_upload_carries_the_file_name() {
        // An explicit name wins.
        let named = MockTransport::new(vec![
            token_ok("TOKEN-A"),
            upload_with_file_info("FI-2"),
            message_sent("m-2"),
        ]);
        let media = url_media(
            MediaKind::Document,
            "https://cdn.example.test/files/q1.pdf?sig=abc",
        )
        .with_filename("quarterly.pdf");
        assert!(
            adapter(&named)
                .send_media("openid_xyz", &media, None)
                .await
                .success
        );
        let requests = named.requests();
        assert_eq!(requests.len(), 3);
        assert_eq!(
            requests[1].url,
            "https://api.sgroup.qq.com/v2/users/openid_xyz/files"
        );
        assert_eq!(
            request_body(&requests[1]),
            json!({
                "file_type": MEDIA_TYPE_FILE,
                "srv_send_msg": false,
                "url": "https://cdn.example.test/files/q1.pdf?sig=abc",
                "file_name": "quarterly.pdf",
            })
        );

        // Without one the URL's last path segment is used (query/fragment excluded)…
        let derived = MockTransport::new(vec![
            token_ok("TOKEN-A"),
            upload_with_file_info("FI-3"),
            message_sent("m-3"),
        ]);
        let media = url_media(
            MediaKind::Document,
            "https://cdn.example.test/files/q2.pdf?sig=abc",
        );
        assert!(
            adapter(&derived)
                .send_media("openid_xyz", &media, None)
                .await
                .success
        );
        assert_eq!(
            request_body(&derived.requests()[1])["file_name"],
            json!("q2.pdf")
        );

        // …and a URL with no path at all falls back to the literal `media`.
        let bare = MockTransport::new(vec![
            token_ok("TOKEN-A"),
            upload_with_file_info("FI-4"),
            message_sent("m-4"),
        ]);
        let media = url_media(MediaKind::Document, "https://cdn.example.test");
        assert!(
            adapter(&bare)
                .send_media("openid_xyz", &media, None)
                .await
                .success
        );
        assert_eq!(
            request_body(&bare.requests()[1])["file_name"],
            json!("media")
        );
    }

    #[tokio::test]
    async fn media_message_body_carries_file_info_seq_caption_and_anchor() {
        let mock = MockTransport::new(vec![
            token_ok("TOKEN-A"),
            upload_with_file_info("FILE-INFO-9"),
            message_sent("m-9"),
        ]);
        let media =
            url_media(MediaKind::Image, "https://cdn.example.test/a.png").with_caption("look");
        let result = adapter(&mock)
            .send_media("openid_xyz", &media, Some("msg-3"))
            .await;

        assert!(result.success);
        assert_eq!(result.message_id.as_deref(), Some("m-9"));
        // The delivery response is kept, like `_post_message`'s `raw_response`.
        assert_eq!(
            result.raw_response.as_ref().and_then(|raw| raw.get("id")),
            Some(&json!("m-9"))
        );

        let requests = mock.requests();
        assert_eq!(requests.len(), 3);
        assert_eq!(
            requests[2].url,
            "https://api.sgroup.qq.com/v2/users/openid_xyz/messages"
        );
        assert_eq!(header(&requests[2], "Authorization"), Some("QQBot TOKEN-A"));
        assert_eq!(
            request_body(&requests[2]),
            json!({
                "msg_type": 7,
                "media": { "file_info": "FILE-INFO-9" },
                "msg_seq": 1,
                "content": "look",
                "msg_id": "msg-3",
            })
        );

        // A blank caption and an absent anchor leave both fields out entirely.
        let plain = MockTransport::new(vec![
            token_ok("TOKEN-A"),
            upload_with_file_info("FI-1"),
            message_sent("m-1"),
        ]);
        let media =
            url_media(MediaKind::Image, "https://cdn.example.test/a.png").with_caption("   ");
        assert!(
            adapter(&plain)
                .send_media("openid_xyz", &media, None)
                .await
                .success
        );
        assert_eq!(
            request_body(&plain.requests()[2]),
            json!({
                "msg_type": 7,
                "media": { "file_info": "FI-1" },
                "msg_seq": 1,
            })
        );
    }

    #[tokio::test]
    async fn media_caption_is_capped_at_the_message_cap() {
        let long = "é".repeat(QQ_MAX_MESSAGE_LENGTH + 500);
        let mock = MockTransport::new(vec![
            token_ok("TOKEN-A"),
            upload_with_file_info("FI-1"),
            message_sent("m-1"),
        ]);
        let media =
            url_media(MediaKind::Image, "https://cdn.example.test/a.png").with_caption(&long);
        assert!(
            adapter(&mock)
                .send_media("openid_xyz", &media, None)
                .await
                .success
        );

        // `caption[: MAX_MESSAGE_LENGTH]` counts code points, so the multi-byte caption survives
        // the cap intact instead of being split mid-character.
        let content = content_of(&mock.requests()[2]);
        assert_eq!(content.chars().count(), QQ_MAX_MESSAGE_LENGTH);
        assert!(content.chars().all(|ch| ch == 'é'));
    }

    #[tokio::test]
    async fn media_file_info_is_read_from_the_upload_or_its_data_object() {
        // Top-level `file_info`.
        let top = MockTransport::new(vec![
            token_ok("TOKEN-A"),
            upload_ok(json!({ "file_info": "TOP-LEVEL", "ttl": 60 })),
            message_sent("m-1"),
        ]);
        let media = url_media(MediaKind::Image, "https://cdn.example.test/a.png");
        assert!(
            adapter(&top)
                .send_media("openid_xyz", &media, None)
                .await
                .success
        );
        assert_eq!(
            request_body(&top.requests()[2])["media"]["file_info"],
            json!("TOP-LEVEL")
        );

        // Nested under `data`, as the reference's fallback reads it.
        let nested = MockTransport::new(vec![
            token_ok("TOKEN-A"),
            upload_ok(json!({ "data": { "file_info": { "file_uuid": "NESTED" } } })),
            message_sent("m-2"),
        ]);
        assert!(
            adapter(&nested)
                .send_media("openid_xyz", &media, None)
                .await
                .success
        );
        assert_eq!(
            request_body(&nested.requests()[2])["media"]["file_info"],
            json!({ "file_uuid": "NESTED" })
        );
    }

    #[tokio::test]
    async fn media_upload_without_a_usable_file_info_fails_typed() {
        for upload in [
            json!({}),
            json!({ "file_info": null }),
            json!({ "file_info": "   " }),
            json!({ "file_info": {} }),
            json!({ "data": {} }),
            json!({ "data": null }),
        ] {
            let mock = MockTransport::new(vec![token_ok("TOKEN-A"), upload_ok(upload.clone())]);
            let media = url_media(MediaKind::Image, "https://cdn.example.test/a.png");
            let result = adapter(&mock).send_media("openid_xyz", &media, None).await;

            assert!(!result.success, "{upload} must not report success");
            assert_eq!(result.error_kind.as_deref(), Some("unknown"), "{upload}");
            assert!(!result.retryable);
            assert!(
                result
                    .error
                    .as_deref()
                    .unwrap_or_default()
                    .contains("no file_info"),
                "the failure names the missing field: {:?}",
                result.error
            );
            // Only the token fetch and the upload; no media message is attempted.
            assert_eq!(mock.requests().len(), 2, "{upload}");
        }
    }

    #[tokio::test]
    async fn guild_media_send_is_refused_without_a_request() {
        let mock = MockTransport::new(Vec::new());
        let media = url_media(MediaKind::Image, "https://cdn.example.test/a.png");
        for chat_id in ["channel_42", "guild_7"] {
            let result = adapter(&mock).send_media(chat_id, &media, None).await;
            assert!(!result.success);
            // The reference's exact wording (`_send_media`'s guild branch).
            assert_eq!(
                result.error.as_deref(),
                Some("Guild media send not supported via this path")
            );
            assert_eq!(result.error_kind.as_deref(), Some("bad_format"));
            assert!(!result.retryable);
        }
        assert!(mock.requests().is_empty());
    }

    #[tokio::test]
    async fn empty_media_url_is_refused_without_a_request() {
        let mock = MockTransport::new(Vec::new());
        let media = url_media(MediaKind::Image, "   ");
        let result = adapter(&mock).send_media("openid_xyz", &media, None).await;
        assert!(!result.success);
        assert_eq!(result.error_kind.as_deref(), Some("bad_format"));
        assert!(
            result
                .error
                .as_deref()
                .unwrap_or_default()
                .contains("empty media url")
        );
        assert!(mock.requests().is_empty());
    }

    #[tokio::test]
    async fn bytes_media_is_refused_naming_the_missing_uploader() {
        let mock = MockTransport::new(Vec::new());
        let media = OutboundMedia::new(MediaKind::Image, MediaSource::Bytes(vec![0u8; 2048]));
        let result = adapter(&mock).send_media("openid_xyz", &media, None).await;

        assert!(!result.success);
        assert_eq!(result.error_kind.as_deref(), Some("bad_format"));
        assert!(!result.retryable);
        let error = result.error.as_deref().unwrap_or_default();
        // The refusal names the source that does work and the uploader that is missing.
        assert!(error.contains("MediaSource::Bytes"), "{error}");
        assert!(error.contains("MediaSource::Url"), "{error}");
        assert!(error.contains("upload_prepare"), "{error}");
        assert!(error.contains("upload_part_finish"), "{error}");
        assert!(error.contains("2048 bytes"), "{error}");
        // Zero requests: no half-upload, no fake success.
        assert!(mock.requests().is_empty());
    }

    #[tokio::test]
    async fn non_http_media_url_is_refused_without_a_request() {
        let mock = MockTransport::new(Vec::new());
        for url in ["/tmp/pic.png", "file:///tmp/pic.png", "ftp://host/pic.png"] {
            let media = url_media(MediaKind::Image, url);
            let result = adapter(&mock).send_media("openid_xyz", &media, None).await;
            assert!(!result.success, "{url} must be refused");
            assert_eq!(result.error_kind.as_deref(), Some("bad_format"));
            assert!(
                result
                    .error
                    .as_deref()
                    .unwrap_or_default()
                    .contains("only fetches http(s)")
            );
        }
        assert!(mock.requests().is_empty());
    }

    #[tokio::test]
    async fn media_upload_rate_limit_is_retryable_with_the_server_delay() {
        let mock = MockTransport::new(vec![
            token_ok("TOKEN-A"),
            Ok(HttpResponse {
                status: 429,
                body: json!({ "code": 11244, "message": "too many requests" }).to_string(),
                retry_after: Some(7.5),
            }),
        ]);
        let media = url_media(MediaKind::Image, "https://cdn.example.test/a.png");
        let result = adapter(&mock)
            .send_media("group_openid_1", &media, None)
            .await;

        assert!(!result.success);
        assert_eq!(result.error_kind.as_deref(), Some("rate_limited"));
        assert!(result.retryable);
        assert_eq!(result.retry_after, Some(7.5));
        assert!(
            result
                .error
                .as_deref()
                .unwrap_or_default()
                .contains("too many requests")
        );
        // No media message follows a refused upload.
        assert_eq!(mock.requests().len(), 2);
    }

    #[tokio::test]
    async fn media_upload_failures_follow_the_send_taxonomy() {
        let cases: [(Result<HttpResponse, TransportError>, &str, bool); 7] = [
            (chat_error(400, 10_001, "bad request"), "bad_format", false),
            (chat_error(401, 10_002, "unauthorized"), "forbidden", false),
            (chat_error(403, 10_003, "forbidden"), "forbidden", false),
            (
                chat_error(404, 40_034, "target not found"),
                "not_found",
                false,
            ),
            // A file QQ refuses as too large is a plain 4xx: resending it cannot help.
            (
                chat_error(413, 10_004, "payload too large"),
                "bad_format",
                false,
            ),
            (chat_error(500, 500_000, "internal"), "transient", true),
            // 413 is covered above; a 2xx that still carries a non-zero `code` is a rejection.
            (
                response(200, json!({ "code": 10_005, "message": "no" })),
                "bad_format",
                false,
            ),
        ];
        for (scripted, expected_kind, expected_retryable) in cases {
            let mock = MockTransport::new(vec![token_ok("TOKEN-A"), scripted]);
            let media = url_media(MediaKind::Image, "https://cdn.example.test/a.png");
            let result = adapter(&mock).send_media("openid_xyz", &media, None).await;

            assert!(!result.success);
            assert_eq!(
                result.error_kind.as_deref(),
                Some(expected_kind),
                "unexpected kind for {:?}",
                result.error
            );
            assert_eq!(
                result.retryable, expected_retryable,
                "unexpected retryable for {:?}",
                result.error
            );
            assert!(result.message_id.is_none());
            // The refused upload never becomes a media message.
            assert_eq!(mock.requests().len(), 2);
        }

        // A transport failure is transient: nothing reached QQ.
        let down = MockTransport::new(vec![
            token_ok("TOKEN-A"),
            Err(TransportError::Request("connection reset".to_string())),
        ]);
        let media = url_media(MediaKind::Image, "https://cdn.example.test/a.png");
        let result = adapter(&down).send_media("openid_xyz", &media, None).await;
        assert_eq!(result.error_kind.as_deref(), Some("transient"));
        assert!(result.retryable);
        assert!(
            result
                .error
                .as_deref()
                .unwrap_or_default()
                .contains("media upload transport error")
        );
    }

    #[tokio::test]
    async fn media_message_failures_are_classified_and_drop_the_token_cache() {
        // The upload succeeds; the media message is refused because the token expired.
        let mock = MockTransport::new(vec![
            token_ok("TOKEN-A"),
            upload_with_file_info("FI-1"),
            chat_error(401, 11_241, "token not exist or expire"),
            token_ok("TOKEN-B"),
            message_sent("m-1"),
        ]);
        let subject = adapter(&mock);
        let media = url_media(MediaKind::Image, "https://cdn.example.test/a.png");

        let refused = subject.send_media("openid_xyz", &media, None).await;
        assert!(!refused.success);
        assert_eq!(refused.error_kind.as_deref(), Some("forbidden"));
        assert!(!refused.retryable);
        assert!(refused.message_id.is_none());

        // The cached token really was dropped: the next send fetches a new one.
        assert!(subject.send("openid_xyz", "hi", None, None).await.success);
        let requests = mock.requests();
        assert_eq!(requests.len(), 5);
        assert_eq!(requests[3].url, QQ_TOKEN_URL);
        assert_eq!(header(&requests[4], "Authorization"), Some("QQBot TOKEN-B"));

        // A 5xx on the media message is transient.
        let server = MockTransport::new(vec![
            token_ok("TOKEN-A"),
            upload_with_file_info("FI-1"),
            chat_error(503, 500_000, "unavailable"),
        ]);
        let result = adapter(&server)
            .send_media("openid_xyz", &media, None)
            .await;
        assert_eq!(result.error_kind.as_deref(), Some("transient"));
        assert!(result.retryable);

        // So is a transport failure on the media message.
        let down = MockTransport::new(vec![
            token_ok("TOKEN-A"),
            upload_with_file_info("FI-1"),
            Err(TransportError::Request("closed".to_string())),
        ]);
        let result = adapter(&down).send_media("openid_xyz", &media, None).await;
        assert_eq!(result.error_kind.as_deref(), Some("transient"));
        assert!(result.retryable);
    }

    #[test]
    fn supports_media_is_true_for_the_richmedia_flow() {
        let mock = MockTransport::new(Vec::new());
        assert!(adapter(&mock).supports_media());
    }

    #[test]
    fn file_type_mapping_matches_the_reference_constants() {
        // constants.py: MEDIA_TYPE_IMAGE=1, _VIDEO=2, _VOICE=3, _FILE=4.
        assert_eq!(file_type_for(MediaKind::Image), 1);
        assert_eq!(file_type_for(MediaKind::Video), 2);
        assert_eq!(file_type_for(MediaKind::Audio), 3);
        assert_eq!(file_type_for(MediaKind::Document), 4);
        assert_eq!(MSG_TYPE_MEDIA, 7);
    }

    #[test]
    fn rest_path_routes_users_and_groups_and_skips_guild() {
        assert_eq!(
            rest_path(ChatKind::Group, "group_open_1", "files").as_deref(),
            Some("/v2/groups/group_open_1/files")
        );
        assert_eq!(
            rest_path(ChatKind::Dm, "openid_xyz", "files").as_deref(),
            Some("/v2/users/openid_xyz/files")
        );
        // The path segment is escaped, so an id cannot re-shape the request path.
        assert_eq!(
            rest_path(ChatKind::Dm, "a/b", "files").as_deref(),
            Some("/v2/users/a%2Fb/files")
        );
        // `_rest_path` has no guild arm: guild media is refused instead.
        assert_eq!(rest_path(ChatKind::Guild, "channel_1", "files"), None);
    }

    #[test]
    fn upload_and_media_bodies_only_carry_the_optional_fields_when_set() {
        assert_eq!(
            upload_body(MEDIA_TYPE_IMAGE, "https://x.test/a.png", None),
            json!({ "file_type": 1, "srv_send_msg": false, "url": "https://x.test/a.png" })
        );
        assert_eq!(
            upload_body(MEDIA_TYPE_FILE, "https://x.test/a.pdf", Some("a.pdf"))["file_name"],
            json!("a.pdf")
        );
        assert_eq!(
            media_body(&json!("FI"), None, None),
            json!({ "msg_type": 7, "media": { "file_info": "FI" }, "msg_seq": 1 })
        );
        assert_eq!(
            media_body(&json!("FI"), Some("cap"), Some(" msg-1 ")),
            json!({
                "msg_type": 7,
                "media": { "file_info": "FI" },
                "msg_seq": 1,
                "content": "cap",
                "msg_id": "msg-1",
            })
        );
    }

    #[test]
    fn file_info_extraction_accepts_both_shapes_only_when_usable() {
        assert_eq!(
            file_info_of(&json!({ "file_info": "FI" })),
            Some(json!("FI"))
        );
        assert_eq!(
            file_info_of(&json!({ "data": { "file_info": "FI" } })),
            Some(json!("FI"))
        );
        // Top-level wins over `data`, like the reference's `or` chain.
        assert_eq!(
            file_info_of(&json!({ "file_info": "TOP", "data": { "file_info": "INNER" } })),
            Some(json!("TOP"))
        );
        for empty in [
            json!({}),
            json!({ "file_info": null }),
            json!({ "file_info": "" }),
            json!({ "file_info": {} }),
            json!({ "data": null }),
            json!({ "data": {} }),
            json!({ "data": { "file_info": [] } }),
            json!({ "file_info": false }),
        ] {
            assert_eq!(file_info_of(&empty), None, "{empty} is not a file_info");
        }
    }

    #[test]
    fn url_last_path_segment_reads_the_name_and_ignores_query_and_fragment() {
        assert_eq!(
            url_last_path_segment("https://cdn.test/dir/a.png").as_deref(),
            Some("a.png")
        );
        assert_eq!(
            url_last_path_segment("https://cdn.test/dir/a.png?sig=1#frag").as_deref(),
            Some("a.png")
        );
        assert_eq!(url_last_path_segment("https://cdn.test/").as_deref(), None);
        assert_eq!(url_last_path_segment("https://cdn.test"), None);
        assert_eq!(url_last_path_segment("   "), None);
    }

    #[test]
    fn document_file_name_prefers_the_supplied_name_then_the_url() {
        let url = "https://cdn.test/dir/report.pdf";
        let named = url_media(MediaKind::Document, url).with_filename(" q1.pdf ");
        assert_eq!(document_file_name(&named, url), "q1.pdf");
        // A blank name is no name at all.
        let blank = url_media(MediaKind::Document, url).with_filename("  ");
        assert_eq!(document_file_name(&blank, url), "report.pdf");
        // An image is never asked for a name, but the fallback chain still resolves one.
        let bare = url_media(MediaKind::Document, "https://cdn.test");
        assert_eq!(document_file_name(&bare, "https://cdn.test"), "media");
    }

    #[test]
    fn http_urls_are_the_only_sources_qq_fetches() {
        assert!(is_http_url("http://x.test/a.png"));
        assert!(is_http_url("HTTPS://x.test/a.png"));
        assert!(is_http_url("  https://x.test/a.png  "));
        for other in [
            "",
            "   ",
            "/tmp/a.png",
            "file:///tmp/a.png",
            "ftp://x.test/a",
        ] {
            assert!(!is_http_url(other), "{other:?} is not an http(s) url");
        }
    }

    #[test]
    fn captions_are_capped_by_code_points_not_bytes() {
        assert_eq!(cap_codepoints("abc", 10), "abc");
        assert_eq!(cap_codepoints("abcdef", 3), "abc");
        // Multi-byte characters are counted, not their bytes: no panic, no split character.
        assert_eq!(cap_codepoints("éééé", 2), "éé");
        assert_eq!(cap_codepoints("😀😀", 1), "😀");
        assert_eq!(cap_codepoints("", 0), "");
    }

    #[tokio::test]
    async fn get_chat_info_falls_back_to_the_id() {
        let mock = MockTransport::new(Vec::new());
        let info = adapter(&mock).get_chat_info("group_abc").await;
        assert_eq!(info.name, "group_abc");
        assert_eq!(info.chat_type, "group");
        // No endpoint exists, so no request is made.
        assert!(mock.requests().is_empty());

        let other = adapter(&mock).get_chat_info("openid_xyz").await;
        assert_eq!(other.name, "openid_xyz");
        assert_eq!(other.chat_type, "group");
    }

    #[tokio::test]
    async fn typing_and_delete_are_noops_and_edit_is_an_honest_failure() {
        let mock = MockTransport::new(Vec::new());
        let adapter = adapter(&mock);
        adapter.send_typing("channel_5", None).await;
        adapter.delete_message("channel_5", "m-1").await;
        let edited = adapter
            .edit_message("channel_5", "m-1", "new text", true)
            .await;

        assert!(!edited.success);
        assert_eq!(edited.error_kind.as_deref(), Some("bad_format"));
        assert!(
            edited
                .error
                .as_deref()
                .unwrap_or_default()
                .contains("does not support editing")
        );
        // Neither operation has an endpoint to call.
        assert!(mock.requests().is_empty());
    }

    #[test]
    fn adapter_capability_flags_match_the_qq_surface() {
        let mock = MockTransport::new(Vec::new());
        let adapter = adapter(&mock);
        assert_eq!(adapter.max_message_length(), QQ_MAX_MESSAGE_LENGTH);
        assert!(adapter.splits_long_messages());
        assert!(adapter.supports_code_blocks());
        assert_eq!(adapter.message_length("héllo"), 5);
    }

    #[test]
    fn infer_chat_kind_reads_the_id_shape() {
        assert_eq!(infer_chat_kind("group_openid_1"), ChatKind::Group);
        assert_eq!(infer_chat_kind("1234567890"), ChatKind::Group);
        assert_eq!(infer_chat_kind("channel_1"), ChatKind::Guild);
        assert_eq!(infer_chat_kind("guild_1"), ChatKind::Guild);
        assert_eq!(infer_chat_kind("openid_xyz"), ChatKind::Dm);
        assert_eq!(infer_chat_kind("user_1"), ChatKind::Dm);
        assert_eq!(infer_chat_kind(""), ChatKind::Dm);
        // A caller hint wins over the id shape, and unknown hints fall back to it.
        assert_eq!(
            chat_kind("group_1", Some(&json!({ "chat_type": "guild" }))),
            ChatKind::Guild
        );
        assert_eq!(
            chat_kind("group_1", Some(&json!({ "chat_type": "something" }))),
            ChatKind::Group
        );
        assert_eq!(chat_kind("group_1", None), ChatKind::Group);
    }

    #[test]
    fn strip_leading_mention_removes_both_marker_spellings() {
        assert_eq!(strip_leading_mention("<@!1234> hello"), "hello");
        assert_eq!(strip_leading_mention("<@1234>hello"), "hello");
        assert_eq!(strip_leading_mention("  <@!1>  two spaces"), "two spaces");
        assert_eq!(strip_leading_mention("plain text"), "plain text");
        assert_eq!(strip_leading_mention("  padded"), "padded");
        // An unterminated marker is not a mention.
        assert_eq!(strip_leading_mention("<@1234"), "<@1234");
        assert_eq!(strip_leading_mention(""), "");
    }

    #[test]
    fn parse_webhook_reads_an_at_message_create() {
        let mock = MockTransport::new(Vec::new());
        let body = json!({
            "op": 0,
            "t": "AT_MESSAGE_CREATE",
            "s": 42,
            "d": {
                "id": "msg-1",
                "channel_id": "channel_9",
                "guild_id": "guild_1",
                "content": "<@!1234> hello from the guild",
                "timestamp": 1_700_000_000_123i64,
                "author": { "id": "u-1", "username": "Ann" },
            },
        })
        .to_string();

        let events = adapter(&mock)
            .parse_webhook(&body)
            .expect("a dispatch frame parses");
        assert_eq!(events.len(), 1);
        let event = &events[0];
        assert_eq!(event.text, "hello from the guild");
        assert_eq!(event.message_id.as_deref(), Some("msg-1"));
        assert_eq!(event.platform_update_id, Some(1_700_000_000_123));
        assert_eq!(event.user_id.as_deref(), Some("u-1"));
        assert_eq!(event.user_name.as_deref(), Some("Ann"));
        assert_eq!(event.source.platform.as_str(), "qqbot");
        assert_eq!(event.source.chat_id, "channel_9");
        assert_eq!(event.source.chat_type, "guild");
        assert_eq!(
            event.metadata.get("chat_type").and_then(Value::as_str),
            Some("guild")
        );
        // The raw dispatch payload is kept for adapter-specific handling.
        assert_eq!(
            event
                .raw_message
                .as_ref()
                .and_then(|raw| raw.get("guild_id"))
                .and_then(Value::as_str),
            Some("guild_1")
        );
        // Parsing never talks to the network.
        assert!(mock.requests().is_empty());
    }

    #[test]
    fn parse_webhook_reads_a_group_at_message() {
        let mock = MockTransport::new(Vec::new());
        let body = json!({
            "op": 0,
            "t": "GROUP_AT_MESSAGE_CREATE",
            "d": {
                "id": "msg-2",
                "group_openid": "group_open_1",
                "content": "<@!555> ping",
                "timestamp": "1700000000456",
                "author": { "member_openid": "m-1", "username": "Bo" },
            },
        })
        .to_string();

        let events = adapter(&mock).parse_webhook(&body).expect("parsed");
        assert_eq!(events.len(), 1);
        let event = &events[0];
        assert_eq!(event.text, "ping");
        assert_eq!(event.source.chat_id, "group_open_1");
        assert_eq!(event.source.chat_type, "group");
        assert_eq!(event.user_id.as_deref(), Some("m-1"));
        assert_eq!(event.user_name.as_deref(), Some("Bo"));
        // A numeric-string timestamp still yields a numeric update id.
        assert_eq!(event.platform_update_id, Some(1_700_000_000_456));
        assert_eq!(
            event.metadata.get("chat_type").and_then(Value::as_str),
            Some("group")
        );
    }

    #[test]
    fn parse_webhook_reads_a_c2c_message() {
        let mock = MockTransport::new(Vec::new());
        let body = json!({
            "op": 0,
            "t": "C2C_MESSAGE_CREATE",
            "d": {
                "id": "msg-3",
                "content": "hi bot",
                "author": { "id": "u-9", "username": "Cy", "user_openid": "u-9" },
            },
        })
        .to_string();

        let events = adapter(&mock).parse_webhook(&body).expect("parsed");
        assert_eq!(events.len(), 1);
        let event = &events[0];
        assert_eq!(event.text, "hi bot");
        // The C2C chat id is the author's own id.
        assert_eq!(event.source.chat_id, "u-9");
        assert_eq!(event.source.chat_type, "dm");
        assert_eq!(event.user_id.as_deref(), Some("u-9"));
        // No timestamp in the frame leaves the update id unset.
        assert_eq!(event.platform_update_id, None);
        assert_eq!(
            event.metadata.get("chat_type").and_then(Value::as_str),
            Some("dm")
        );
    }

    #[test]
    fn parse_webhook_ignores_unrelated_payloads() {
        let mock = MockTransport::new(Vec::new());
        let adapter = adapter(&mock);
        let dispatch = |t: &str, d: Value| json!({ "op": 0, "t": t, "d": d }).to_string();

        // Control frames: Hello, heartbeat ACK, server reconnect, invalid session.
        for body in [
            json!({ "op": 10, "d": { "heartbeat_interval": 30000 } }).to_string(),
            json!({ "op": 11 }).to_string(),
            json!({ "op": 7 }).to_string(),
            json!({ "op": 9, "d": true }).to_string(),
        ] {
            assert!(
                adapter
                    .parse_webhook(&body)
                    .expect("control frame")
                    .is_empty(),
                "control frame must not produce an event: {body}"
            );
        }
        // Dispatch types this adapter has no event shape for.
        for event_type in [
            "READY",
            "RESUMED",
            "INTERACTION_CREATE",
            "GUILD_MESSAGE_CREATE",
        ] {
            let body = dispatch(event_type, json!({ "id": "x", "content": "hi" }));
            assert!(
                adapter.parse_webhook(&body).expect("skipped").is_empty(),
                "{event_type} must be skipped"
            );
        }
        // A supported frame with nothing to answer is skipped too.
        for data in [
            json!({ "id": "m", "channel_id": "channel_1", "content": "   " }),
            json!({
                "id": "m",
                "channel_id": "channel_1",
                "content": "<@!1>",
                "author": { "id": "u" },
            }),
            json!({ "id": "m", "content": "no chat id anywhere" }),
            json!({ "channel_id": "channel_1", "content": "no author" }),
        ] {
            assert!(
                adapter
                    .parse_webhook(&dispatch("AT_MESSAGE_CREATE", data))
                    .expect("skipped")
                    .is_empty()
            );
        }
        // Hostile input is an error, not a silent skip.
        assert!(adapter.parse_webhook("not json").is_err());
        assert!(adapter.parse_webhook("").is_err());
        assert!(mock.requests().is_empty());
    }

    // ---- QQ webhook signatures: Ed25519 over `timestamp ‖ body`, keyed by the client secret ----

    /// A 32-byte ASCII `client_secret`: QQ's webhook signing seed is the app secret, verbatim.
    const WEBHOOK_SECRET: &str = "0123456789abcdef0123456789abcdef";

    /// A second secret of the same shape, to prove the key decides the answer.
    const OTHER_SECRET: &str = "fedcba9876543210fedcba9876543210";

    /// The `X-Signature-Timestamp` value used by these tests.
    const WEBHOOK_TIMESTAMP: &str = "1700000000";

    /// The raw request body that gets signed.
    const WEBHOOK_BODY: &[u8] = br#"{"op":0,"t":"AT_MESSAGE_CREATE","d":{"id":"m-1"}}"#;

    fn seed_of(secret: &str) -> [u8; ED25519_SEED_LEN] {
        let mut seed = [0u8; ED25519_SEED_LEN];
        seed.copy_from_slice(secret.as_bytes());
        seed
    }

    fn webhook_seed() -> [u8; ED25519_SEED_LEN] {
        seed_of(WEBHOOK_SECRET)
    }

    /// An adapter holding `secret`, over a transport that must never be used.
    fn webhook_adapter(secret: &str) -> QqBotAdapter {
        let mock = MockTransport::new(Vec::new());
        QqBotAdapter::new(mock, "APP-ID", secret)
    }

    fn hex_of(bytes: &[u8]) -> String {
        bytes.iter().map(|byte| format!("{byte:02x}")).collect()
    }

    fn key_pair_for(seed: &[u8; ED25519_SEED_LEN]) -> Ed25519KeyPair {
        Ed25519KeyPair::from_seed_unchecked(seed).expect("32 bytes is a valid Ed25519 seed")
    }

    /// `M = timestamp ‖ body`, exactly what QQ signs.
    fn signed_message(timestamp: &str, body: &[u8]) -> Vec<u8> {
        let mut message = timestamp.as_bytes().to_vec();
        message.extend_from_slice(body);
        message
    }

    /// The `<R ‖ S>` hex QQ puts in `X-Signature-Ed25519`.
    fn sign(seed: &[u8; ED25519_SEED_LEN], timestamp: &str, body: &[u8]) -> String {
        let signature = key_pair_for(seed).sign(&signed_message(timestamp, body));
        hex_of(signature.as_ref())
    }

    /// The same signature with one bit flipped at byte `index`.
    fn with_bit_flipped(signature: &str, index: usize, mask: u8) -> String {
        let mut bytes = decode_hex_bytes(signature).expect("generated signature is valid hex");
        bytes[index] ^= mask;
        hex_of(&bytes)
    }

    /// The same signature with `L` added to `S`: the same scalar mod `L`, but out of range.
    fn with_non_canonical_scalar(signature: &str) -> String {
        let mut bytes = decode_hex_bytes(signature).expect("generated signature is valid hex");
        let mut carry = 0u16;
        for index in 0..ED25519_HALF_LEN {
            let sum = u16::from(bytes[ED25519_HALF_LEN + index])
                + u16::from(ED25519_ORDER_L[index])
                + carry;
            bytes[ED25519_HALF_LEN + index] = sum as u8;
            carry = sum >> 8;
        }
        assert_eq!(carry, 0, "S + L must still fit in 32 bytes");
        hex_of(&bytes)
    }

    /// The same signature with its `R` half replaced by `r`.
    fn with_r(signature: &str, r: [u8; ED25519_HALF_LEN]) -> String {
        let mut bytes = decode_hex_bytes(signature).expect("generated signature is valid hex");
        bytes[..ED25519_HALF_LEN].copy_from_slice(&r);
        hex_of(&bytes)
    }

    /// RFC 8032 §5.1.5 spelled out over `curve25519-dalek`: `h = SHA-512(seed)`, clamp `h[0..32]`,
    /// `a = h[0..32] mod L`, `A = [a]B`. `EdwardsPoint::mul_base` is the base-point multiplication
    /// (`ED25519_BASEPOINT_TABLE × a`).
    fn reference_public_key(seed: &[u8; ED25519_SEED_LEN]) -> [u8; ED25519_HALF_LEN] {
        let digest = digest::digest(&SHA512, seed);
        let mut h = [0u8; ED25519_HALF_LEN];
        h.copy_from_slice(&digest.as_ref()[..ED25519_HALF_LEN]);
        h[0] &= 248;
        h[31] &= 127;
        h[31] |= 64;
        EdwardsPoint::mul_base(&Scalar::from_bytes_mod_order(h))
            .compress()
            .to_bytes()
    }

    /// RFC 8032 §5.1.7 spelled out over `curve25519-dalek`: `k = SHA-512(R ‖ A ‖ M) mod L`,
    /// `R_check = [S]B − [k]A`, accept when `R_check` compresses back to `R`. An independent oracle
    /// for [`QqBotAdapter::verify_webhook`], refusing a non-canonical `S` and undecodable points the
    /// same way the adapter does.
    fn reference_verify(seed: &[u8; ED25519_SEED_LEN], message: &[u8], signature: &[u8]) -> bool {
        let Ok(signature) = <&[u8; ED25519_SIGNATURE_LEN]>::try_from(signature) else {
            return false;
        };
        let mut r = [0u8; ED25519_HALF_LEN];
        r.copy_from_slice(&signature[..ED25519_HALF_LEN]);
        let mut s = [0u8; ED25519_HALF_LEN];
        s.copy_from_slice(&signature[ED25519_HALF_LEN..]);
        if !scalar_is_canonical(&s) {
            return false;
        }
        let public_key = reference_public_key(seed);
        let (Some(r_point), Some(a_point)) = (
            CompressedEdwardsY(r).decompress(),
            CompressedEdwardsY(public_key).decompress(),
        ) else {
            return false;
        };
        let mut input = Vec::with_capacity(2 * ED25519_HALF_LEN + message.len());
        input.extend_from_slice(&r);
        input.extend_from_slice(&public_key);
        input.extend_from_slice(message);
        let digest = digest::digest(&SHA512, &input);
        let mut wide = [0u8; 2 * ED25519_HALF_LEN];
        wide.copy_from_slice(digest.as_ref());
        let k = Scalar::from_bytes_mod_order_wide(&wide);
        // `s` is already known canonical, so this reduction is the identity.
        let challenge_times_key = k * a_point;
        let r_check =
            EdwardsPoint::mul_base(&Scalar::from_bytes_mod_order(s)) - challenge_times_key;
        r_check.compress().to_bytes() == r_point.compress().to_bytes()
    }

    #[test]
    fn verify_webhook_accepts_a_signature_over_timestamp_and_body() {
        let adapter = webhook_adapter(WEBHOOK_SECRET);
        assert!(adapter.has_webhook_secret());
        let signature = sign(&webhook_seed(), WEBHOOK_TIMESTAMP, WEBHOOK_BODY);

        assert!(adapter.verify_webhook(WEBHOOK_TIMESTAMP, &signature, WEBHOOK_BODY));
        // Hex case carries no meaning, and surrounding whitespace is not signature material.
        assert!(adapter.verify_webhook(
            WEBHOOK_TIMESTAMP,
            &format!("  {}\n", signature.to_uppercase()),
            WEBHOOK_BODY
        ));
        // An empty body is a legal message: the signature then covers `timestamp` alone.
        let empty = sign(&webhook_seed(), WEBHOOK_TIMESTAMP, b"");
        assert!(adapter.verify_webhook(WEBHOOK_TIMESTAMP, &empty, b""));
    }

    #[test]
    fn verify_webhook_rejects_a_tampered_body() {
        let adapter = webhook_adapter(WEBHOOK_SECRET);
        let signature = sign(&webhook_seed(), WEBHOOK_TIMESTAMP, WEBHOOK_BODY);
        let tampered = String::from_utf8_lossy(WEBHOOK_BODY).replace("\"m-1\"", "\"m-2\"");
        assert_ne!(tampered.as_bytes(), WEBHOOK_BODY);
        assert!(!adapter.verify_webhook(WEBHOOK_TIMESTAMP, &signature, tampered.as_bytes()));

        // Appending to the body is a change too, even though the JSON still parses.
        let mut extended = WEBHOOK_BODY.to_vec();
        extended.push(b' ');
        assert!(!adapter.verify_webhook(WEBHOOK_TIMESTAMP, &signature, &extended));
        // …and so is truncating it.
        assert!(!adapter.verify_webhook(
            WEBHOOK_TIMESTAMP,
            &signature,
            &WEBHOOK_BODY[..WEBHOOK_BODY.len() - 1]
        ));
    }

    #[test]
    fn verify_webhook_rejects_a_tampered_signature() {
        let adapter = webhook_adapter(WEBHOOK_SECRET);
        let signature = sign(&webhook_seed(), WEBHOOK_TIMESTAMP, WEBHOOK_BODY);

        // One bit in `R`…
        assert!(!adapter.verify_webhook(
            WEBHOOK_TIMESTAMP,
            &with_bit_flipped(&signature, 0, 1),
            WEBHOOK_BODY
        ));
        // …and one bit in `S`, which stays inside the scalar range (so the range check is not what
        // rejects it — the equation is).
        let flipped_s = with_bit_flipped(&signature, ED25519_HALF_LEN, 1);
        let mut s = [0u8; ED25519_HALF_LEN];
        s.copy_from_slice(&decode_hex_bytes(&flipped_s).expect("hex")[ED25519_HALF_LEN..]);
        assert!(
            scalar_is_canonical(&s),
            "the flipped S must stay a range-valid scalar"
        );
        assert!(!adapter.verify_webhook(WEBHOOK_TIMESTAMP, &flipped_s, WEBHOOK_BODY));
        // An all-zero "signature" must never be mistaken for one.
        assert!(!adapter.verify_webhook(WEBHOOK_TIMESTAMP, &"00".repeat(64), WEBHOOK_BODY));
    }

    #[test]
    fn verify_webhook_binds_the_timestamp() {
        let adapter = webhook_adapter(WEBHOOK_SECRET);
        let signature = sign(&webhook_seed(), WEBHOOK_TIMESTAMP, WEBHOOK_BODY);

        assert!(!adapter.verify_webhook("1700000001", &signature, WEBHOOK_BODY));
        // The timestamp is signed as raw bytes, so a re-spelled instant fails.
        assert!(!adapter.verify_webhook("1700000000.0", &signature, WEBHOOK_BODY));
        assert!(!adapter.verify_webhook(" 1700000000", &signature, WEBHOOK_BODY));
        // A signature made over a different timestamp does not verify against this one.
        let other = sign(&webhook_seed(), "1700000001", WEBHOOK_BODY);
        assert!(!adapter.verify_webhook(WEBHOOK_TIMESTAMP, &other, WEBHOOK_BODY));
    }

    #[test]
    fn verify_webhook_rejects_a_signature_from_another_secret() {
        // Signed by the other key, presented to this adapter.
        let foreign = sign(&seed_of(OTHER_SECRET), WEBHOOK_TIMESTAMP, WEBHOOK_BODY);
        assert!(!webhook_adapter(WEBHOOK_SECRET).verify_webhook(
            WEBHOOK_TIMESTAMP,
            &foreign,
            WEBHOOK_BODY
        ));
        // The genuine frame, presented to an adapter configured with the other secret.
        let genuine = sign(&webhook_seed(), WEBHOOK_TIMESTAMP, WEBHOOK_BODY);
        assert!(!webhook_adapter(OTHER_SECRET).verify_webhook(
            WEBHOOK_TIMESTAMP,
            &genuine,
            WEBHOOK_BODY
        ));
    }

    #[test]
    fn verify_webhook_rejects_malformed_hex() {
        let adapter = webhook_adapter(WEBHOOK_SECRET);
        let signature = sign(&webhook_seed(), WEBHOOK_TIMESTAMP, WEBHOOK_BODY);
        // Right length, one character that is not a hex digit.
        let mut poisoned = signature.clone();
        poisoned.replace_range(0..1, "g");
        for bad in [
            String::new(),
            "not-hex".to_string(),
            "zz".repeat(32),
            poisoned,
            format!("0x{signature}"),
            signature[..127].to_string(),
        ] {
            assert!(
                !adapter.verify_webhook(WEBHOOK_TIMESTAMP, &bad, WEBHOOK_BODY),
                "{bad:?} must not verify"
            );
        }
    }

    #[test]
    fn verify_webhook_rejects_a_wrong_length_signature() {
        let adapter = webhook_adapter(WEBHOOK_SECRET);
        let signature = sign(&webhook_seed(), WEBHOOK_TIMESTAMP, WEBHOOK_BODY);
        assert_eq!(signature.len(), 2 * ED25519_SIGNATURE_LEN);
        for bad in [
            // `R` alone: 32 bytes, the length of a point, not of a signature.
            signature[..2 * ED25519_HALF_LEN].to_string(),
            // One byte short.
            signature[..126].to_string(),
            // One byte long.
            format!("{signature}00"),
            // Twice as long, all zeroes.
            "00".repeat(2 * ED25519_SIGNATURE_LEN),
        ] {
            assert!(
                !adapter.verify_webhook(WEBHOOK_TIMESTAMP, &bad, WEBHOOK_BODY),
                "{} hex chars must not verify",
                bad.len()
            );
        }
    }

    #[test]
    fn verify_webhook_rejects_a_secret_that_is_not_a_32_byte_seed() {
        let signature = sign(&webhook_seed(), WEBHOOK_TIMESTAMP, WEBHOOK_BODY);
        let mut extended = String::from(WEBHOOK_SECRET);
        extended.push('x');
        // No zero padding and no truncation: a secret that is not exactly 32 bytes is not a seed,
        // so it derives no key and verifies nothing.
        for secret in [
            String::new(),
            "short".to_string(),
            WEBHOOK_SECRET[..ED25519_SEED_LEN - 1].to_string(),
            extended,
        ] {
            let adapter = webhook_adapter(&secret);
            assert!(!adapter.has_webhook_secret(), "{secret:?} is not a seed");
            assert!(!adapter.verify_webhook(WEBHOOK_TIMESTAMP, &signature, WEBHOOK_BODY));
        }
        assert!(webhook_adapter(WEBHOOK_SECRET).has_webhook_secret());
    }

    #[test]
    fn verify_webhook_rejects_a_non_canonical_scalar() {
        let adapter = webhook_adapter(WEBHOOK_SECRET);
        let signature = sign(&webhook_seed(), WEBHOOK_TIMESTAMP, WEBHOOK_BODY);
        let forged = with_non_canonical_scalar(&signature);

        // `S + L ≡ S (mod L)`: only the range check distinguishes the forgery from the original.
        assert!(!adapter.verify_webhook(WEBHOOK_TIMESTAMP, &forged, WEBHOOK_BODY));
        let mut s = [0u8; ED25519_HALF_LEN];
        s.copy_from_slice(&decode_hex_bytes(&forged).expect("hex")[ED25519_HALF_LEN..]);
        assert!(!scalar_is_canonical(&s), "S + L is out of range");

        // The boundary: `L` itself is out of range, `L - 1` is the largest canonical scalar.
        assert!(!scalar_is_canonical(&ED25519_ORDER_L));
        let mut below_l = ED25519_ORDER_L;
        below_l[0] -= 1;
        assert!(scalar_is_canonical(&below_l));
        assert!(scalar_is_canonical(&[0u8; ED25519_HALF_LEN]));
    }

    #[test]
    fn verify_webhook_rejects_invalid_points_without_panicking() {
        let adapter = webhook_adapter(WEBHOOK_SECRET);
        let signature = sign(&webhook_seed(), WEBHOOK_TIMESTAMP, WEBHOOK_BODY);
        // `y = p` in little-endian: not a reduced field element, so not a point.
        let mut field_prime = [0xffu8; ED25519_HALF_LEN];
        field_prime[0] = 0xed;
        field_prime[31] = 0x7f;
        for r in [[0xffu8; ED25519_HALF_LEN], field_prime] {
            let bad = with_r(&signature, r);
            assert!(!adapter.verify_webhook(WEBHOOK_TIMESTAMP, &bad, WEBHOOK_BODY));
        }
        // All `0xff`: an undecodable `R` *and* a non-canonical `S` — refused, never a panic.
        assert!(!adapter.verify_webhook(WEBHOOK_TIMESTAMP, &"ff".repeat(64), WEBHOOK_BODY));
    }

    /// The load-bearing check: QQ's documented derivation (`h = SHA-512(seed)`, clamp,
    /// `A = [a]B`) must produce, byte for byte, the key `verify_webhook` verifies against.
    #[test]
    fn webhook_public_key_is_the_rfc8032_derivation_of_the_secret() {
        for seed in [
            webhook_seed(),
            seed_of(OTHER_SECRET),
            [0u8; ED25519_SEED_LEN],
            [0xffu8; ED25519_SEED_LEN],
            [7u8; ED25519_SEED_LEN],
        ] {
            let derived = webhook_public_key(&seed).expect("a 32-byte seed derives a key");
            assert_eq!(derived, reference_public_key(&seed));
            let ring_key = key_pair_for(&seed);
            let ring_bytes: &[u8] = ring_key.public_key().as_ref();
            assert_eq!(&derived[..], ring_bytes);
            // …and it really is a compressed curve point, not just equal to ring's bytes.
            assert_eq!(
                CompressedEdwardsY(derived)
                    .decompress()
                    .map(|point| point.compress().to_bytes()),
                Some(derived)
            );
        }
    }

    /// `verify_webhook` and the reference §5.1.7 math must agree on every input — accept and reject
    /// alike — so the ring-backed production path cannot drift from the scheme QQ documents.
    #[test]
    fn verify_webhook_agrees_with_the_reference_ed25519_math() {
        let adapter = webhook_adapter(WEBHOOK_SECRET);
        let seed = webhook_seed();
        let genuine = sign(&seed, WEBHOOK_TIMESTAMP, WEBHOOK_BODY);
        let mut flipped = decode_hex_bytes(&genuine).expect("hex");
        flipped[3] ^= 0x08;
        let cases = [
            (WEBHOOK_TIMESTAMP, WEBHOOK_BODY.to_vec(), genuine.clone()),
            (WEBHOOK_TIMESTAMP, b"{\"op\":0}".to_vec(), genuine.clone()),
            ("1700000001", WEBHOOK_BODY.to_vec(), genuine.clone()),
            (WEBHOOK_TIMESTAMP, WEBHOOK_BODY.to_vec(), hex_of(&flipped)),
            (
                WEBHOOK_TIMESTAMP,
                WEBHOOK_BODY.to_vec(),
                with_non_canonical_scalar(&genuine),
            ),
            (
                WEBHOOK_TIMESTAMP,
                WEBHOOK_BODY.to_vec(),
                with_r(&genuine, [0xff; ED25519_HALF_LEN]),
            ),
            (
                WEBHOOK_TIMESTAMP,
                WEBHOOK_BODY.to_vec(),
                sign(&seed_of(OTHER_SECRET), WEBHOOK_TIMESTAMP, WEBHOOK_BODY),
            ),
        ];
        let reference_accepts = |(timestamp, body, signature): &(&str, Vec<u8>, String)| {
            let decoded = decode_hex_bytes(signature).expect("case signatures are valid hex");
            reference_verify(&seed, &signed_message(timestamp, body), &decoded)
        };
        for case in &cases {
            assert_eq!(
                adapter.verify_webhook(case.0, &case.2, &case.1),
                reference_accepts(case),
                "disagreement on the case signed for {case:?}"
            );
        }
        // Exactly one case is a genuine signature, so the agreement above is not vacuous.
        assert_eq!(
            cases.iter().filter(|case| reference_accepts(case)).count(),
            1
        );
    }
}
