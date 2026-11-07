//! Weixin (WeChat) adapter — Tencent iLink Bot API.
//!
//! The bot speaks JSON over HTTPS at `ilinkai.weixin.qq.com`: `ilink/bot/sendmessage` to deliver
//! text, `ilink/bot/getupdates` to long-poll inbound messages. Every reply must echo the peer's
//! latest `context_token`; the adapter caches one per chat and retries once without it when iLink
//! reports the session stale.
//!
//! Outbound media is ported from the reference's `_send_file` path (`adapter.py:1142-1190`):
//! `ilink/bot/getuploadurl`, an AES-128-ECB + PKCS#7 encrypted POST to the Weixin CDN, then the
//! caption and the media `item_list` message. [`PlatformAdapter::send_media`] carries the
//! phase-by-phase mapping and the few deliberate deviations.
//!
//! Not ported: inbound media download/decrypt, QR-code login and account linking, typing tickets,
//! native outbound voice (`.silk`) item shapes, and the persistent on-disk context-token store
//! (this adapter keeps tokens in memory).

use aes::cipher::generic_array::GenericArray;
use aes::cipher::{BlockEncrypt, KeyInit};
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
use crate::transport::{HttpRequest, HttpResponse, SharedTransport};

pub const WEIXIN_MAX_MESSAGE_LENGTH: usize = 2000;
const APP_ID: &str = "bot";
/// `(2 << 16) | (2 << 8) | 0`, the client version iLink expects.
const APP_CLIENT_VERSION: &str = "131584";
const ITEM_TEXT: i64 = 1;
const MSG_TYPE_BOT: i64 = 2;
const MSG_STATE_FINISH: i64 = 2;

/// `getuploadurl` `media_type` values (`adapter.py:60`): image, video, file. The reference's voice
/// value (4) is only used for `.silk` local paths (`adapter.py:1192-1200`), and outbound voice is
/// always forced to a file attachment anyway (`adapter.py:1128-1130`), so this adapter never sends it.
const MEDIA_TYPE_IMAGE: i64 = 1;
const MEDIA_TYPE_VIDEO: i64 = 2;
const MEDIA_TYPE_FILE: i64 = 3;
/// `item_list` entry types (`adapter.py:61`). Text is [`ITEM_TEXT`]; voice (3) is inbound-only here.
const ITEM_IMAGE: i64 = 2;
const ITEM_FILE: i64 = 4;
const ITEM_VIDEO: i64 = 5;
/// The CDN that accepts the AES-128-ECB ciphertext (`adapter.py:51`, `WEIXIN_CDN_BASE_URL`).
pub const WEIXIN_CDN_BASE_URL: &str = "https://novac2c.cdn.weixin.qq.com/c2c";
/// `secrets.token_hex(16)` file key (`adapter.py:1146`).
const FILEKEY_BYTES: usize = 16;
/// AES-128 key length: `secrets.token_bytes(16)` (`adapter.py:1146`).
const AES_KEY_BYTES: usize = 16;
/// AES block / PKCS#7 block size (`adapter.py:132-139`).
const AES_BLOCK_BYTES: usize = 16;
/// Largest payload this adapter will upload.
///
/// The reference imposes no byte cap of its own — its only size arithmetic is the PKCS#7-rounded
/// `filesize` (`adapter.py:1146-1150`) — so the ceiling here is the one the wire format itself has:
/// `rawsize` and `filesize` are signed 32-bit JSON integers.
pub const WEIXIN_MEDIA_MAX_BYTES: usize = i32::MAX as usize;
/// iLink's stale-session return code (`SESSION_EXPIRED_ERRCODE`, `adapter.py:58`).
const SESSION_EXPIRED_ERRCODE: i64 = -14;
/// iLink's frequency-limit code (`RATE_LIMIT_ERRCODE`, `adapter.py:58`).
const RATE_LIMIT_ERRCODE: i64 = -2;

static CLIENT_SEQ: AtomicU64 = AtomicU64::new(0);

pub struct WeixinAdapter {
    transport: SharedTransport,
    base_url: String,
    cdn_base_url: String,
    account_id: String,
    token: String,
    context_tokens: Mutex<HashMap<String, String>>,
}

impl WeixinAdapter {
    pub fn new(
        transport: SharedTransport,
        base_url: impl Into<String>,
        account_id: impl Into<String>,
        token: impl Into<String>,
    ) -> Self {
        Self {
            transport,
            base_url: base_url.into().trim_end_matches('/').to_string(),
            cdn_base_url: WEIXIN_CDN_BASE_URL.to_string(),
            account_id: account_id.into(),
            token: token.into(),
            context_tokens: Mutex::new(HashMap::new()),
        }
    }

    /// Point uploads at another CDN base (the reference's `extra.cdn_base_url`, `adapter.py:720`).
    pub fn with_cdn_base_url(mut self, cdn_base_url: impl Into<String>) -> Self {
        self.cdn_base_url = cdn_base_url.into().trim_end_matches('/').to_string();
        self
    }

    /// Remember the peer's latest `context_token`; a reply must echo it.
    pub fn remember_context_token(&self, chat_id: &str, token: &str) {
        if let Ok(mut map) = self.context_tokens.lock() {
            map.insert(chat_id.to_string(), token.to_string());
        }
    }

    fn context_token(&self, chat_id: &str) -> Option<String> {
        self.context_tokens
            .lock()
            .ok()
            .and_then(|map| map.get(chat_id).cloned())
    }

    /// Forget a `context_token` iLink rejected as stale (the reference pops it from its store,
    /// `adapter.py:1183`), so the next send for this peer starts tokenless.
    fn forget_context_token(&self, chat_id: &str) {
        if let Ok(mut map) = self.context_tokens.lock() {
            map.remove(chat_id);
        }
    }

    fn endpoint(&self, path: &str) -> String {
        format!("{}/{path}", self.base_url)
    }

    fn headers(&self, request: HttpRequest) -> HttpRequest {
        // iLink wants a per-request obfuscated user number; any decimal string works, base64'd.
        let uin = base64_encode(
            (CLIENT_SEQ.fetch_add(1, Ordering::Relaxed) + 1)
                .to_string()
                .as_bytes(),
        );
        request
            .header("AuthorizationType", "ilink_bot_token")
            .header("X-WECHAT-UIN", uin)
            .header("iLink-App-Id", APP_ID)
            .header("iLink-App-ClientVersion", APP_CLIENT_VERSION)
            .bearer(&self.token)
    }

    async fn post(&self, path: &str, body: &Value) -> Result<Value, String> {
        let request = self.headers(HttpRequest::json(self.endpoint(path), body));
        let response = self
            .transport
            .execute(request)
            .await
            .map_err(|err| format!("weixin transport error: {err}"))?;
        let value = response.json().unwrap_or(Value::Null);
        if !response.ok() {
            return Err(format!(
                "weixin returned {}: {}",
                response.status,
                truncate(&response.body, 200)
            ));
        }
        Ok(value)
    }

    async fn send_chunk(
        &self,
        chat_id: &str,
        text: &str,
        context_token: Option<&str>,
    ) -> Result<String, String> {
        let client_id = next_client_id();
        let mut message = json!({
            "from_user_id": "",
            "to_user_id": chat_id,
            "client_id": client_id,
            "message_type": MSG_TYPE_BOT,
            "message_state": MSG_STATE_FINISH,
            "item_list": [{"type": ITEM_TEXT, "text_item": {"text": text}}],
        });
        if let Some(token) = context_token {
            message["context_token"] = json!(token);
        }
        let reply = self
            .post("ilink/bot/sendmessage", &json!({ "msg": message }))
            .await?;
        let ret = reply.get("ret").and_then(Value::as_i64).unwrap_or(0);
        let errcode = reply.get("errcode").and_then(Value::as_i64).unwrap_or(0);
        if ret != 0 || errcode != 0 {
            return Err(format!(
                "weixin sendmessage ret={ret} errcode={errcode}: {}",
                reply
                    .get("errmsg")
                    .and_then(Value::as_str)
                    .unwrap_or("unknown error")
            ));
        }
        Ok(client_id)
    }

    /// Perform one request of the media pipeline, mapping transport and status failures onto a
    /// typed [`MediaFailure`] that names the phase, so a failure never lets a later phase run.
    async fn media_request(
        &self,
        phase: MediaPhase,
        request: HttpRequest,
    ) -> Result<HttpResponse, MediaFailure> {
        match self.transport.execute(request).await {
            Ok(response) if response.ok() => Ok(response),
            Ok(response) => {
                let (kind, retryable) = classify_status(response.status);
                let failure = MediaFailure::new(
                    phase,
                    kind,
                    format!(
                        "weixin {} HTTP {}: {}",
                        phase.as_str(),
                        response.status,
                        truncate(&response.body, 200)
                    ),
                );
                Err(if retryable {
                    failure.retryable(response.retry_after)
                } else {
                    failure
                })
            }
            Err(err) => Err(MediaFailure::new(
                phase,
                "transient",
                format!("weixin {} transport error: {err}", phase.as_str()),
            )
            .retryable(None)),
        }
    }

    /// The reference's whole outbound media path (`_send_file`, `adapter.py:1142-1190`).
    ///
    /// Returns the message ids in send order: the caption first when there is one, then the media
    /// message. Every step returns early on failure, so phases after a failed one never run.
    async fn send_media_bytes(
        &self,
        chat_id: &str,
        kind: MediaKind,
        filename: &str,
        caption: Option<&str>,
        plaintext: &[u8],
    ) -> Result<Vec<String>, MediaFailure> {
        let (media_type, item_type) = media_shape(kind);
        let aes_key = random_aes_key().map_err(|err| {
            MediaFailure::new(
                MediaPhase::UploadUrl,
                "unknown",
                format!("weixin could not draw the AES-128 key: {err}"),
            )
        })?;
        let filekey = random_hex(FILEKEY_BYTES).map_err(|err| {
            MediaFailure::new(
                MediaPhase::UploadUrl,
                "unknown",
                format!("weixin could not draw the file key: {err}"),
            )
        })?;

        let rawsize = plaintext.len();
        let rawfilemd5 = md5_hex(plaintext);
        let filesize = padded_size(rawsize);
        // `getuploadurl` payload, field for field (`adapter.py:333-340`, called at 1148-1150). The
        // reference's `_api_post` also wraps every body in `base_info.channel_version`; like the
        // text path in this adapter (`post`), this port sends the payload alone.
        let payload = json!({
            "filekey": filekey,
            "media_type": media_type,
            "to_user_id": chat_id,
            "rawsize": rawsize,
            "rawfilemd5": rawfilemd5,
            "filesize": filesize,
            "no_need_thumb": true,
            "aeskey": hex_encode(&aes_key),
        });
        let request = self.headers(HttpRequest::json(
            self.endpoint("ilink/bot/getuploadurl"),
            &payload,
        ));
        let response = self.media_request(MediaPhase::UploadUrl, request).await?;
        let upload = response.json().map_err(|err| {
            MediaFailure::new(
                MediaPhase::UploadUrl,
                "unknown",
                format!("weixin getuploadurl returned an unreadable body: {err}"),
            )
        })?;
        let upload_url = upload_url_from(&self.cdn_base_url, &upload, &filekey)
            .map_err(|err| MediaFailure::new(MediaPhase::UploadUrl, "unknown", err))?;

        // AES-128-ECB + PKCS#7 over the plaintext (`adapter.py:1152`), then the ciphertext POST
        // (`_upload_ciphertext`, `adapter.py:343-352`).
        let ciphertext = aes128_ecb_encrypt(plaintext, &aes_key)
            .map_err(|err| MediaFailure::new(MediaPhase::CdnUpload, "bad_format", err))?;
        let request = HttpRequest::post(upload_url)
            .header("Content-Type", "application/octet-stream")
            .body(ciphertext.clone());
        let uploaded = self.media_request(MediaPhase::CdnUpload, request).await?;
        // Only a 200 carries the media token (`adapter.py:346-351`).
        if uploaded.status != 200 {
            let (kind, retryable) = classify_status(uploaded.status);
            let failure = MediaFailure::new(
                MediaPhase::CdnUpload,
                kind,
                format!(
                    "weixin CDN upload HTTP {}: {}",
                    uploaded.status,
                    truncate(&uploaded.body, 200)
                ),
            );
            return Err(if retryable {
                failure.retryable(uploaded.retry_after)
            } else {
                failure
            });
        }
        let encrypt_query_param = cdn_encrypt_query_param(&uploaded)
            .map_err(|err| MediaFailure::new(MediaPhase::CdnUpload, "unknown", err))?;

        // iLink wants `aes_key` as base64(hex string), not base64(raw bytes) — otherwise images
        // render as grey boxes (`adapter.py:1160-1163`).
        let aes_key_for_api = base64_encode(hex_encode(&aes_key).as_bytes());
        let item = media_item(
            item_type,
            &encrypt_query_param,
            &aes_key_for_api,
            ciphertext.len(),
            rawsize,
            filename,
            &rawfilemd5,
        );

        let mut context_token = self.context_token(chat_id);
        let mut ids = Vec::new();
        if let Some(caption) = caption.filter(|caption| !caption.trim().is_empty()) {
            // The caption travels as its own preceding text message, formatted like any text send
            // (`adapter.py:1167-1168`).
            let text = self.format_message(caption);
            let item_list = json!([{"type": ITEM_TEXT, "text_item": {"text": text}}]);
            ids.push(
                self.deliver_item_list(chat_id, &item_list, &mut context_token)
                    .await?,
            );
        }
        ids.push(
            self.deliver_item_list(chat_id, &json!([item]), &mut context_token)
                .await?,
        );
        Ok(ids)
    }

    /// One `ilink/bot/sendmessage` for an `item_list` (`_send_items`, `adapter.py:306-314`), with the
    /// reference's stale-session fallback: a `-14`/`-2` rejection re-sends once without the cached
    /// token, forgets it, and that tokenless state carries into the remaining item lists
    /// (`adapter.py:1170-1190`).
    async fn deliver_item_list(
        &self,
        chat_id: &str,
        item_list: &Value,
        context_token: &mut Option<String>,
    ) -> Result<String, MediaFailure> {
        let client_id = next_client_id();
        loop {
            let mut message = json!({
                "from_user_id": "",
                "to_user_id": chat_id,
                "client_id": client_id,
                "message_type": MSG_TYPE_BOT,
                "message_state": MSG_STATE_FINISH,
                "item_list": item_list,
            });
            if let Some(token) = context_token.as_deref() {
                message["context_token"] = json!(token);
            }
            let request = self.headers(HttpRequest::json(
                self.endpoint("ilink/bot/sendmessage"),
                &json!({ "msg": message }),
            ));
            let response = self
                .media_request(MediaPhase::MediaMessage, request)
                .await?;
            let reply = response.json().unwrap_or(Value::Null);
            let ret = reply.get("ret").and_then(Value::as_i64).unwrap_or(0);
            let errcode = reply.get("errcode").and_then(Value::as_i64).unwrap_or(0);
            if ret == 0 && errcode == 0 {
                return Ok(client_id);
            }
            let errmsg = reply
                .get("errmsg")
                .and_then(Value::as_str)
                .unwrap_or("")
                .to_string();
            if is_session_expired(ret, errcode) && context_token.is_some() {
                *context_token = None;
                self.forget_context_token(chat_id);
                continue;
            }
            // `-2` with "unknown error"/"prepare failed" after the tokenless re-send is iLink
            // refusing a bot-initiated send until the peer messages first: deterministic, not a
            // rate limit (`_session_not_ready_error`, `adapter.py:82-89`, 1187-1189).
            if is_stale_session_ret(ret, errcode, &errmsg) {
                return Err(MediaFailure::new(
                    MediaPhase::MediaMessage,
                    "forbidden",
                    format!(
                        "iLink sendmessage session not ready: ret={ret} errcode={errcode} errmsg={} \
                         — the user must send the bot a message first (or re-pair)",
                        if errmsg.is_empty() {
                            "unknown error"
                        } else {
                            errmsg.as_str()
                        }
                    ),
                ));
            }
            let detail = format!(
                "iLink sendmessage error: ret={ret} errcode={errcode} errmsg={}",
                if errmsg.is_empty() {
                    "unknown error"
                } else {
                    errmsg.as_str()
                }
            );
            return Err(
                if ret == RATE_LIMIT_ERRCODE || errcode == RATE_LIMIT_ERRCODE {
                    MediaFailure::new(MediaPhase::MediaMessage, "rate_limited", detail)
                } else {
                    MediaFailure::new(MediaPhase::MediaMessage, "transient", detail)
                }
                .retryable(None),
            );
        }
    }

    /// Long-poll inbound messages. Returns the new sync cursor alongside the events.
    pub async fn poll_once(
        &self,
        sync_buf: Option<&str>,
    ) -> Result<(Vec<MessageEvent>, String), String> {
        let body = json!({ "get_updates_buf": sync_buf.unwrap_or("") });
        let reply = self.post("ilink/bot/getupdates", &body).await?;
        let next = reply
            .get("get_updates_buf")
            .and_then(Value::as_str)
            .unwrap_or_default()
            .to_string();
        let mut events = Vec::new();
        let platform = Platform::new("weixin").expect("valid platform");
        if let Some(messages) = reply.get("msgs").and_then(Value::as_array) {
            for message in messages {
                if let Some(event) = self.parse_message(message, &platform) {
                    events.push(event);
                }
            }
        }
        Ok((events, next))
    }

    fn parse_message(&self, message: &Value, platform: &Platform) -> Option<MessageEvent> {
        let from = message
            .get("from_user_id")
            .and_then(Value::as_str)
            .unwrap_or_default();
        if from.is_empty() {
            return None;
        }
        let room = message
            .get("room_id")
            .and_then(Value::as_str)
            .filter(|room| !room.is_empty());
        let items = message
            .get("item_list")
            .and_then(Value::as_array)
            .cloned()
            .unwrap_or_default();
        let text = items
            .iter()
            .find(|item| item.get("type").and_then(Value::as_i64) == Some(ITEM_TEXT))
            .and_then(|item| item.get("text_item"))
            .and_then(|text| text.get("text"))
            .and_then(Value::as_str)
            .unwrap_or_default()
            .to_string();
        if text.trim().is_empty() {
            return None;
        }

        let chat_id = room.unwrap_or(from).to_string();
        let mut source = SessionSource::new(platform.clone(), chat_id.clone());
        source.chat_type = if room.is_some() { "group" } else { "dm" }.to_string();
        source.user_id = Some(from.to_string());
        source.message_id = message
            .get("msg_id")
            .or_else(|| message.get("client_id"))
            .and_then(Value::as_str)
            .map(str::to_owned);

        let mut event = MessageEvent::new(text, source);
        event.user_id = event.source.user_id.clone();
        event.message_id = event.source.message_id.clone();
        event.message_type = MessageType::Text;
        event.platform_update_id = message.get("create_time").and_then(Value::as_i64);
        event.raw_message = Some(message.clone());
        if let Some(token) = message.get("context_token").and_then(Value::as_str) {
            self.remember_context_token(&chat_id, token);
            event
                .metadata
                .insert("context_token".to_string(), json!(token));
        }
        Some(event)
    }
}

#[async_trait]
impl PlatformAdapter for WeixinAdapter {
    async fn connect(&mut self, _is_reconnect: bool) -> bool {
        !self.token.trim().is_empty() && !self.account_id.trim().is_empty()
    }

    async fn disconnect(&mut self) {
        if let Ok(mut map) = self.context_tokens.lock() {
            map.clear();
        }
    }

    async fn send(
        &self,
        chat_id: &str,
        content: &str,
        _reply_to: Option<&str>,
        _metadata: Option<Value>,
    ) -> SendResult {
        if chat_id.trim().is_empty() {
            return SendResult::failed("weixin needs a recipient", Some("bad_format"));
        }
        if content.trim().is_empty() {
            return SendResult::failed("empty message content", Some("bad_format"));
        }
        let chunks = truncate_message(content, WEIXIN_MAX_MESSAGE_LENGTH, codepoint_len);
        let mut ids = Vec::new();
        for chunk in chunks {
            let token = self.context_token(chat_id);
            let attempt = self.send_chunk(chat_id, &chunk, token.as_deref()).await;
            let id = match attempt {
                Ok(id) => id,
                Err(err) => {
                    // A stale session (`ret -14`, or `-2`) accepts a tokenless resend.
                    if token.is_some() && (err.contains("ret=-14") || err.contains("-2")) {
                        match self.send_chunk(chat_id, &chunk, None).await {
                            Ok(id) => id,
                            Err(inner) => {
                                return SendResult::failed(inner, Some("transient"))
                                    .retryable(None);
                            }
                        }
                    } else if err.contains("rate") {
                        return SendResult::failed(err, Some("rate_limited")).retryable(None);
                    } else {
                        return SendResult::failed(err, Some("transient")).retryable(None);
                    }
                }
            };
            ids.push(id);
        }
        match ids.last() {
            Some(last) => SendResult::ok_split(last.clone(), ids),
            None => SendResult::ok(""),
        }
    }

    fn splits_long_messages(&self) -> bool {
        true
    }

    /// Weixin uploads through the AES-128-ECB encrypted CDN (`_send_file`, `adapter.py:1142-1190`),
    /// so [`MediaSource::Bytes`] is genuinely supported.
    fn supports_media(&self) -> bool {
        true
    }

    /// Upload one attachment and send it as an iLink media message.
    ///
    /// The reference path, phase by phase (`_send_file`, `adapter.py:1142-1190`):
    ///
    /// 1. `ilink/bot/getuploadurl` with `filekey`, `media_type` (1 image / 2 video / 3 file),
    ///    `to_user_id`, `rawsize`, `rawfilemd5`, the PKCS#7-rounded `filesize`, `no_need_thumb` and
    ///    the hex `aeskey` — a fresh 16-byte AES key and a `secrets.token_hex(16)` file key
    ///    (`adapter.py:333-340`, 1146-1150);
    /// 2. AES-128-ECB with PKCS#7 over the payload (`_aes128_ecb_encrypt`, `adapter.py:132-139`) and
    ///    a **POST** of the ciphertext to `upload_full_url`, or to
    ///    `{cdn}/upload?encrypted_query_param=…&filekey=…` when only `upload_param` came back, with
    ///    `Content-Type: application/octet-stream` as the only header (`adapter.py:343-352`,
    ///    1153-1158);
    /// 3. the CDN's media token, then the caption as a preceding text message and the media
    ///    `item_list` entry carrying `media.encrypt_query_param`, base64(hex) `media.aes_key`,
    ///    `encrypt_type: 1` plus the per-type size/name fields (`adapter.py:661-679`, 1161-1190).
    ///
    /// Deviations, all fail-safe:
    /// - [`MediaSource::Url`] is refused with zero requests: the reference downloads the URL itself
    ///   (`adapter.py:1095-1105`, 1132-1140) and this adapter has no binary fetcher — upload bytes
    ///   through [`MediaSource::Bytes`] instead.
    /// - `reply_to` is dropped, exactly as the reference drops it: `send_document` accepts
    ///   `reply_to` and never encodes it (`adapter.py:1119-1123`).
    /// - The CDN token is read from the response body, because the frozen `Transport` seam surfaces
    ///   no response headers (see [`cdn_encrypt_query_param`]).
    /// - Audio ships as a file attachment, mirroring `send_voice`'s
    ///   `force_file_attachment=True` (`adapter.py:1128-1130`).
    /// - `media.mime` is not consulted: [`MediaKind`] already carries the type the reference derives
    ///   from the mime, and `mime`/`filename` only ever influence its name (`adapter.py:1192-1200`,
    ///   1163).
    ///
    /// Any phase's failure returns its own typed [`SendResult`] with `raw_response.phase` set, and
    /// no later phase is attempted.
    async fn send_media(
        &self,
        chat_id: &str,
        media: &OutboundMedia,
        reply_to: Option<&str>,
    ) -> SendResult {
        let _ = reply_to;
        if chat_id.trim().is_empty() {
            return SendResult::failed("weixin needs a recipient", Some("bad_format"));
        }
        let plaintext = match &media.source {
            MediaSource::Bytes(bytes) => bytes.as_slice(),
            MediaSource::Url(url) => {
                return SendResult::failed(
                    format!(
                        "weixin send_media uploads bytes only: this adapter has no binary fetcher \
                         for MediaSource::Url ({url:?}), which the reference downloads itself \
                         (adapter.py:1095-1105, 1132-1140) — pass MediaSource::Bytes"
                    ),
                    Some("bad_format"),
                );
            }
        };
        if let Some((kind, message)) = media_size_error(plaintext.len()) {
            return SendResult::failed(message, Some(kind));
        }
        let filename = media
            .filename
            .as_deref()
            .map(str::trim)
            .filter(|name| !name.is_empty())
            .map(str::to_owned)
            .unwrap_or_else(|| default_filename(media.kind));
        match self
            .send_media_bytes(
                chat_id,
                media.kind,
                &filename,
                media.caption.as_deref(),
                plaintext,
            )
            .await
        {
            Ok(ids) => match ids.last() {
                // Ids in send order; `message_id` stays the LAST one (the media message).
                Some(last) => SendResult::ok_split(last.clone(), ids),
                None => {
                    SendResult::failed("weixin media send produced no message", Some("unknown"))
                }
            },
            Err(failure) => failure.into_send_result(),
        }
    }

    fn max_message_length(&self) -> usize {
        WEIXIN_MAX_MESSAGE_LENGTH
    }

    async fn get_chat_info(&self, chat_id: &str) -> ChatInfo {
        ChatInfo {
            name: chat_id.to_string(),
            chat_type: "dm".to_string(),
        }
    }
}

fn truncate(text: &str, limit: usize) -> String {
    if text.chars().count() <= limit {
        return text.to_string();
    }
    text.chars().take(limit).collect::<String>() + "…"
}

/// Standard base64 with padding; iLink only needs it for the obfuscated user number.
fn base64_encode(input: &[u8]) -> String {
    const ALPHABET: &[u8; 64] = b"ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz0123456789+/";
    let mut out = String::with_capacity(input.len().div_ceil(3) * 4);
    for chunk in input.chunks(3) {
        let b0 = chunk[0] as u32;
        let b1 = *chunk.get(1).unwrap_or(&0) as u32;
        let b2 = *chunk.get(2).unwrap_or(&0) as u32;
        let triple = (b0 << 16) | (b1 << 8) | b2;
        out.push(ALPHABET[((triple >> 18) & 0x3f) as usize] as char);
        out.push(ALPHABET[((triple >> 12) & 0x3f) as usize] as char);
        out.push(if chunk.len() > 1 {
            ALPHABET[((triple >> 6) & 0x3f) as usize] as char
        } else {
            '='
        });
        out.push(if chunk.len() > 2 {
            ALPHABET[(triple & 0x3f) as usize] as char
        } else {
            '='
        });
    }
    out
}

// ----- media: reference path helpers -----

/// Where in the reference's media path a send stopped (`_send_file`, `adapter.py:1142-1190`).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum MediaPhase {
    /// `ilink/bot/getuploadurl` — the upload ticket (`adapter.py:1148-1157`).
    UploadUrl,
    /// The ciphertext POST to the Weixin CDN (`adapter.py:343-352`, 1158).
    CdnUpload,
    /// The caption and/or media `ilink/bot/sendmessage` (`adapter.py:1170-1190`).
    MediaMessage,
}

impl MediaPhase {
    fn as_str(self) -> &'static str {
        match self {
            Self::UploadUrl => "upload_url",
            Self::CdnUpload => "cdn_upload",
            Self::MediaMessage => "media_message",
        }
    }
}

/// One typed media failure: the phase that failed, its [`SendResult`] error kind, and the retry hint.
///
/// Failures are values, not panics: every phase returns one instead of unwrapping wire data.
#[derive(Debug)]
struct MediaFailure {
    phase: MediaPhase,
    kind: &'static str,
    message: String,
    retryable: bool,
    retry_after: Option<f64>,
}

impl MediaFailure {
    fn new(phase: MediaPhase, kind: &'static str, message: impl Into<String>) -> Self {
        Self {
            phase,
            kind,
            message: message.into(),
            retryable: false,
            retry_after: None,
        }
    }

    fn retryable(mut self, retry_after: Option<f64>) -> Self {
        self.retryable = true;
        self.retry_after = retry_after;
        self
    }

    /// The typed [`SendResult`]: taxonomy kind, plus the failing phase in `raw_response` so callers
    /// (and the tests) can see that no later phase ran.
    fn into_send_result(self) -> SendResult {
        let mut result = SendResult::failed(self.message, Some(self.kind));
        result.raw_response = Some(json!({
            "phase": self.phase.as_str(),
            "error_kind": self.kind,
            "retryable": self.retryable,
        }));
        if self.retryable {
            result = result.retryable(self.retry_after);
        }
        result
    }
}

/// Map a **non-2xx** HTTP status onto the send-error taxonomy and whether a retry can help:
/// 400 → `bad_format`, 403 → `forbidden`, 404 → `not_found`, 413 → `too_long`,
/// 429 → `rate_limited` (retryable), 5xx → `transient` (retryable), anything else → `unknown`.
fn classify_status(status: u16) -> (&'static str, bool) {
    match status {
        400 => ("bad_format", false),
        403 => ("forbidden", false),
        404 => ("not_found", false),
        413 => ("too_long", false),
        429 => ("rate_limited", true),
        500..=599 => ("transient", true),
        _ => ("unknown", false),
    }
}

/// `MediaKind` → (`getuploadurl` `media_type`, `item_list` entry type).
///
/// The reference derives both from the file's mime and name (`_outbound_media_builder`,
/// `adapter.py:1192-1200`): `image/*` → image, `video/*` → video, `.silk` → voice, everything else
/// — `audio/*` included — → file. Native outbound voice is deliberately unused (`send_voice` forces
/// a file attachment, `adapter.py:1128-1130`), so [`MediaKind::Audio`] ships as a file.
fn media_shape(kind: MediaKind) -> (i64, i64) {
    match kind {
        MediaKind::Image => (MEDIA_TYPE_IMAGE, ITEM_IMAGE),
        MediaKind::Video => (MEDIA_TYPE_VIDEO, ITEM_VIDEO),
        MediaKind::Audio | MediaKind::Document => (MEDIA_TYPE_FILE, ITEM_FILE),
    }
}

/// Filename when the caller supplied none.
///
/// The reference always sends a real path and uses its last segment (`Path(path).name`,
/// `adapter.py:1163`); with bare bytes there is no path, so fall back to the names its inbound cache
/// already uses (`adapter.py:685-688`) and `document.bin`, its file default (`adapter.py:932`).
fn default_filename(kind: MediaKind) -> String {
    match kind {
        MediaKind::Image => "image.jpg",
        MediaKind::Video => "video.mp4",
        MediaKind::Audio => "audio.bin",
        MediaKind::Document => "document.bin",
    }
    .to_string()
}

/// One `item_list` entry for an uploaded file (`_media_item`, `adapter.py:661-679`, filled in at
/// `adapter.py:1161-1166`).
///
/// The shared `media` block is `{encrypt_query_param, aes_key, encrypt_type: 1}`; each type then
/// carries its own fields. `file_item.len` is a **string** (`str(kw["plaintext_size"])`,
/// `adapter.py:662`) while the image/video sizes are numbers — mirroring that exactly matters.
fn media_item(
    item_type: i64,
    encrypt_query_param: &str,
    aes_key_for_api: &str,
    ciphertext_size: usize,
    plaintext_size: usize,
    filename: &str,
    rawfilemd5: &str,
) -> Value {
    let media = json!({
        "encrypt_query_param": encrypt_query_param,
        "aes_key": aes_key_for_api,
        "encrypt_type": 1,
    });
    match item_type {
        ITEM_IMAGE => json!({
            "type": item_type,
            "image_item": {"media": media, "mid_size": ciphertext_size},
        }),
        ITEM_VIDEO => json!({
            "type": item_type,
            "video_item": {
                "media": media,
                "video_size": ciphertext_size,
                "play_length": 0,
                "video_md5": rawfilemd5,
            },
        }),
        _ => json!({
            "type": item_type,
            "file_item": {
                "media": media,
                "file_name": filename,
                "len": plaintext_size.to_string(),
            },
        }),
    }
}

/// The CDN URL the ciphertext is POSTed to (`adapter.py:1153-1157`).
///
/// `upload_full_url` wins (direct CDN); otherwise the reference builds
/// `{cdn}/upload?encrypted_query_param={quote(upload_param)}&filekey={quote(filekey)}` and fails when
/// the response carried neither. POST, never PUT — the CDN 404s a PUT (`adapter.py:1153`).
fn upload_url_from(cdn_base_url: &str, upload: &Value, filekey: &str) -> Result<String, String> {
    if let Some(full) = upload
        .get("upload_full_url")
        .and_then(Value::as_str)
        .map(str::trim)
        .filter(|url| !url.is_empty())
    {
        return Ok(full.to_string());
    }
    match upload
        .get("upload_param")
        .and_then(Value::as_str)
        .filter(|param| !param.is_empty())
    {
        Some(param) => Ok(format!(
            "{}/upload?encrypted_query_param={}&filekey={}",
            cdn_base_url.trim_end_matches('/'),
            percent_encode(param),
            percent_encode(filekey)
        )),
        None => Err(format!(
            "getUploadUrl returned neither upload_param nor upload_full_url: {upload}"
        )),
    }
}

/// The media token a successful CDN upload yields (`_upload_ciphertext`, `adapter.py:343-352`).
///
/// The reference reads it from the `x-encrypted-param` **response header**. The frozen
/// [`Transport`](crate::transport::Transport) seam hands adapters a header-less
/// [`HttpResponse`], so that header is not reachable from an adapter; iLink's CDN also echoes the
/// same token as the bare response body, which is what this reads. The body is accepted only when it
/// is exactly one opaque token — never markup, never JSON, never whitespace-separated — and anything
/// else fails the phase rather than sending a media message with an invented
/// `encrypt_query_param`.
fn cdn_encrypt_query_param(response: &HttpResponse) -> Result<String, String> {
    let token = response.body.trim();
    let plausible = !token.is_empty()
        && token.len() <= 1024
        && token.bytes().all(|byte| {
            matches!(
                byte,
                b'A'..=b'Z'
                    | b'a'..=b'z'
                    | b'0'..=b'9'
                    | b'-'
                    | b'_'
                    | b'.'
                    | b'~'
                    | b'='
                    | b'+'
                    | b'/'
                    | b'%'
            )
        });
    if plausible {
        Ok(token.to_string())
    } else {
        Err(format!(
            "CDN upload returned no x-encrypted-param token (the Transport seam exposes no response \
             headers); body: {}",
            truncate(&response.body, 200)
        ))
    }
}

/// `((rawsize + 16) // 16) * 16` — the reference's `filesize` (`adapter.py:1150`).
///
/// This is exactly the PKCS#7 ciphertext length: unaligned payloads round up to the next block, and
/// an already-aligned payload gains a whole 16-byte pad block.
fn padded_size(rawsize: usize) -> usize {
    ((rawsize + AES_BLOCK_BYTES) / AES_BLOCK_BYTES) * AES_BLOCK_BYTES
}

/// PKCS#7 pad to the 16-byte block (`_pkcs7_pad`, `adapter.py:132-134`): always 1..=16 pad bytes, so
/// an aligned payload grows by a full block.
fn pkcs7_pad(data: &[u8]) -> Vec<u8> {
    let pad_len = AES_BLOCK_BYTES - (data.len() % AES_BLOCK_BYTES);
    let mut padded = Vec::with_capacity(data.len() + pad_len);
    padded.extend_from_slice(data);
    padded.extend(std::iter::repeat_n(pad_len as u8, pad_len));
    padded
}

/// AES-128-ECB over the PKCS#7-padded payload (`_aes128_ecb_encrypt`, `adapter.py:137-139`).
///
/// ECB has no IV: every 16-byte block is encrypted independently with the same key, which is what the
/// Weixin CDN expects. Errors instead of panicking on a bad key or a non-block payload.
fn aes128_ecb_encrypt(plaintext: &[u8], key: &[u8; AES_KEY_BYTES]) -> Result<Vec<u8>, String> {
    let cipher = aes::Aes128::new_from_slice(key)
        .map_err(|err| format!("weixin AES-128 key rejected: {err}"))?;
    let padded = pkcs7_pad(plaintext);
    let mut out = Vec::with_capacity(padded.len());
    for chunk in padded.chunks(AES_BLOCK_BYTES) {
        if chunk.len() != AES_BLOCK_BYTES {
            return Err(format!(
                "weixin AES-128 block was {} bytes, expected {AES_BLOCK_BYTES}",
                chunk.len()
            ));
        }
        let mut block = GenericArray::clone_from_slice(chunk);
        cipher.encrypt_block(&mut block);
        out.extend_from_slice(block.as_slice());
    }
    Ok(out)
}

/// The per-upload AES-128 key: `secrets.token_bytes(16)` (`adapter.py:1146`), drawn from the crate's
/// existing `getrandom` rather than a new dependency.
fn random_aes_key() -> Result<[u8; AES_KEY_BYTES], String> {
    let mut key = [0u8; AES_KEY_BYTES];
    getrandom::fill(&mut key).map_err(|err| format!("weixin entropy unavailable: {err}"))?;
    Ok(key)
}

/// `secrets.token_hex(len)` — `len` random bytes as lowercase hex (`adapter.py:1146`).
fn random_hex(len: usize) -> Result<String, String> {
    let mut bytes = vec![0u8; len];
    getrandom::fill(&mut bytes).map_err(|err| format!("weixin entropy unavailable: {err}"))?;
    Ok(hex_encode(&bytes))
}

/// Lowercase hex, as Python's `bytes.hex()` (`aeskey`, `filekey`, and the hex string the media
/// message base64-encodes).
fn hex_encode(bytes: &[u8]) -> String {
    bytes.iter().map(|byte| format!("{byte:02x}")).collect()
}

/// Python's `quote(value, safe="")` as used for the CDN URL (`adapter.py:1155`): letters, digits and
/// `-_.~` stay literal, every other byte becomes uppercase `%XX`.
fn percent_encode(value: &str) -> String {
    let mut out = String::with_capacity(value.len());
    for byte in value.bytes() {
        match byte {
            b'A'..=b'Z' | b'a'..=b'z' | b'0'..=b'9' | b'-' | b'_' | b'.' | b'~' => {
                out.push(byte as char);
            }
            other => out.push_str(&format!("%{other:02X}")),
        }
    }
    out
}

/// Which media cap a payload breaks, if any, with the [`SendResult`] kind to report.
///
/// An empty payload is refused (there is nothing to encrypt, and PKCS#7 of nothing is a block of pure
/// padding); so is anything the signed 32-bit `rawsize`/`filesize` fields cannot describe. The
/// reference itself has no byte cap (`_send_file`, `adapter.py:1146-1150`), so no smaller cap is
/// invented here.
fn media_size_error(len: usize) -> Option<(&'static str, String)> {
    if len == 0 {
        return Some((
            "bad_format",
            "weixin media payload is empty: there is nothing to upload".to_string(),
        ));
    }
    if len > WEIXIN_MEDIA_MAX_BYTES {
        return Some((
            "too_long",
            format!(
                "weixin media payload is {len} bytes, above the {WEIXIN_MEDIA_MAX_BYTES}-byte wire \
                 limit (rawsize/filesize are 32-bit JSON integers)"
            ),
        ));
    }
    None
}

/// A fresh iLink `client_id` per message; the reference uses `hermes-weixin-{uuid4}`
/// (`adapter.py:1171`), this uses the process id plus the shared sequence so ids stay unique without
/// a uuid dependency.
fn next_client_id() -> String {
    format!(
        "vikram-weixin-{}-{}",
        std::process::id(),
        CLIENT_SEQ.fetch_add(1, Ordering::Relaxed)
    )
}

/// iLink reported the peer's session stale, so a tokenless re-send may work (`_is_session_expired`,
/// `adapter.py:78-79`).
fn is_session_expired(ret: i64, errcode: i64) -> bool {
    ret == SESSION_EXPIRED_ERRCODE || errcode == SESSION_EXPIRED_ERRCODE
}

/// The stale-session variants of iLink's `-2` (`_is_stale_session_ret`, `adapter.py:70-75`): `-2` with
/// `unknown error` / `prepare failed` is a not-ready session, not a real rate limit.
fn is_stale_session_ret(ret: i64, errcode: i64, errmsg: &str) -> bool {
    let limited = ret == RATE_LIMIT_ERRCODE || errcode == RATE_LIMIT_ERRCODE;
    let message = errmsg.trim().to_ascii_lowercase();
    limited && (message == "unknown error" || message == "prepare failed")
}

/// Lowercase hex MD5 of `data` — the `rawfilemd5` field (`hashlib.md5(data).hexdigest()`,
/// `adapter.py:1147`).
fn md5_hex(data: &[u8]) -> String {
    md5::digest(data)
        .iter()
        .map(|byte| format!("{byte:02x}"))
        .collect()
}

/// MD5 (RFC 1321), implemented here because the crate may not add dependencies and no MD5 is
/// otherwise available; `rawfilemd5` is part of the `getuploadurl` contract (`adapter.py:1147`).
/// `wecom.rs` carries the same module for the same reason.
mod md5 {
    /// Per-round left-rotation amounts.
    const SHIFTS: [u32; 64] = [
        7, 12, 17, 22, 7, 12, 17, 22, 7, 12, 17, 22, 7, 12, 17, 22, 5, 9, 14, 20, 5, 9, 14, 20, 5,
        9, 14, 20, 5, 9, 14, 20, 4, 11, 16, 23, 4, 11, 16, 23, 4, 11, 16, 23, 4, 11, 16, 23, 6, 10,
        15, 21, 6, 10, 15, 21, 6, 10, 15, 21, 6, 10, 15, 21,
    ];

    /// `floor(abs(sin(i + 1)) * 2^32)`, the RFC's table.
    const TABLE: [u32; 64] = [
        0xd76a_a478,
        0xe8c7_b756,
        0x2420_70db,
        0xc1bd_ceee,
        0xf57c_0faf,
        0x4787_c62a,
        0xa830_4613,
        0xfd46_9501,
        0x6980_98d8,
        0x8b44_f7af,
        0xffff_5bb1,
        0x895c_d7be,
        0x6b90_1122,
        0xfd98_7193,
        0xa679_438e,
        0x49b4_0821,
        0xf61e_2562,
        0xc040_b340,
        0x265e_5a51,
        0xe9b6_c7aa,
        0xd62f_105d,
        0x0244_1453,
        0xd8a1_e681,
        0xe7d3_fbc8,
        0x21e1_cde6,
        0xc337_07d6,
        0xf4d5_0d87,
        0x455a_14ed,
        0xa9e3_e905,
        0xfcef_a3f8,
        0x676f_02d9,
        0x8d2a_4c8a,
        0xfffa_3942,
        0x8771_f681,
        0x6d9d_6122,
        0xfde5_380c,
        0xa4be_ea44,
        0x4bde_cfa9,
        0xf6bb_4b60,
        0xbebf_bc70,
        0x289b_7ec6,
        0xeaa1_27fa,
        0xd4ef_3085,
        0x0488_1d05,
        0xd9d4_d039,
        0xe6db_99e5,
        0x1fa2_7cf8,
        0xc4ac_5665,
        0xf429_2244,
        0x432a_ff97,
        0xab94_23a7,
        0xfc93_a039,
        0x655b_59c3,
        0x8f0c_cc92,
        0xffef_f47d,
        0x8584_5dd1,
        0x6fa8_7e4f,
        0xfe2c_e6e0,
        0xa301_4314,
        0x4e08_11a1,
        0xf753_7e82,
        0xbd3a_f235,
        0x2ad7_d2bb,
        0xeb86_d391,
    ];

    /// Raw 16-byte digest, little-endian words as the RFC lays them out.
    pub(super) fn digest(data: &[u8]) -> [u8; 16] {
        let mut message = data.to_vec();
        let bit_len = (data.len() as u64).wrapping_mul(8);
        message.push(0x80);
        while message.len() % 64 != 56 {
            message.push(0);
        }
        message.extend_from_slice(&bit_len.to_le_bytes());

        let mut state = [0x6745_2301u32, 0xefcd_ab89, 0x98ba_dcfe, 0x1032_5476];
        for block in message.as_chunks::<64>().0 {
            let mut words = [0u32; 16];
            for (word, chunk) in words.iter_mut().zip(block.as_chunks::<4>().0) {
                *word = u32::from_le_bytes(*chunk);
            }
            let (mut a, mut b, mut c, mut d) = (state[0], state[1], state[2], state[3]);
            for round in 0..64 {
                let (mixed, index) = match round / 16 {
                    0 => ((b & c) | (!b & d), round),
                    1 => ((d & b) | (!d & c), (5 * round + 1) % 16),
                    2 => (b ^ c ^ d, (3 * round + 5) % 16),
                    _ => (c ^ (b | !d), (7 * round) % 16),
                };
                let sum = a
                    .wrapping_add(mixed)
                    .wrapping_add(TABLE[round])
                    .wrapping_add(words[index]);
                a = d;
                d = c;
                c = b;
                b = b.wrapping_add(sum.rotate_left(SHIFTS[round]));
            }
            state[0] = state[0].wrapping_add(a);
            state[1] = state[1].wrapping_add(b);
            state[2] = state[2].wrapping_add(c);
            state[3] = state[3].wrapping_add(d);
        }

        let mut out = [0u8; 16];
        for (chunk, word) in out.as_chunks_mut::<4>().0.iter_mut().zip(state) {
            *chunk = word.to_le_bytes();
        }
        out
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::transport::{HttpMethod, HttpResponse, Transport, TransportError};
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

    fn build(responses: Vec<HttpResponse>) -> (WeixinAdapter, Arc<MockTransport>) {
        let mock = MockTransport::new(responses);
        let adapter = WeixinAdapter::new(
            Arc::clone(&mock) as SharedTransport,
            "https://ilinkai.weixin.qq.com",
            "acct",
            "tok",
        );
        (adapter, mock)
    }

    fn ok() -> HttpResponse {
        HttpResponse {
            status: 200,
            body: json!({"ret": 0, "errcode": 0}).to_string(),
            retry_after: None,
        }
    }

    fn body_of(request: &HttpRequest) -> Value {
        serde_json::from_slice(request.body.as_ref().expect("body")).expect("json")
    }

    /// A 200 whose body is the given JSON value.
    fn json_ok(value: Value) -> HttpResponse {
        HttpResponse {
            status: 200,
            body: value.to_string(),
            retry_after: None,
        }
    }

    /// An arbitrary status with a text body.
    fn status(code: u16, body: &str) -> HttpResponse {
        HttpResponse {
            status: code,
            body: body.to_string(),
            retry_after: None,
        }
    }

    /// A successful CDN upload: a bare media token, which is what
    /// [`cdn_encrypt_query_param`] accepts in place of the unreachable `x-encrypted-param` header.
    fn cdn_ok(token: &str) -> HttpResponse {
        status(200, token)
    }

    /// `getuploadurl`'s answer pointing at a direct CDN URL.
    fn upload_ticket() -> HttpResponse {
        json_ok(json!({"ret": 0, "upload_full_url": "https://cdn.test/upload"}))
    }

    /// Decode lowercase hex in tests (the adapter only ever encodes).
    fn hex_decode(hex: &str) -> Vec<u8> {
        hex.as_bytes()
            .chunks(2)
            .map(|pair| {
                let text = std::str::from_utf8(pair).expect("hex");
                u8::from_str_radix(text, 16).expect("hex")
            })
            .collect()
    }

    /// The 16-byte payload most media tests upload (one full AES block).
    fn payload() -> Vec<u8> {
        vec![0x11; 16]
    }

    fn image_media() -> OutboundMedia {
        OutboundMedia::new(MediaKind::Image, MediaSource::Bytes(payload())).with_filename("cat.png")
    }

    /// The three scripted responses of a successful image upload.
    fn happy_responses() -> Vec<HttpResponse> {
        vec![upload_ticket(), cdn_ok("enc-token-1"), ok()]
    }

    #[test]
    fn base64_matches_known_vectors() {
        assert_eq!(base64_encode(b""), "");
        assert_eq!(base64_encode(b"f"), "Zg==");
        assert_eq!(base64_encode(b"fo"), "Zm8=");
        assert_eq!(base64_encode(b"foo"), "Zm9v");
        assert_eq!(base64_encode(b"hello world"), "aGVsbG8gd29ybGQ=");
    }

    #[tokio::test]
    async fn send_posts_the_ilink_envelope_and_headers() {
        let (adapter, mock) = build(vec![ok()]);
        let result = adapter.send("peer1", "hi", None, None).await;
        assert!(result.success);
        let requests = mock.requests();
        assert_eq!(
            requests[0].url,
            "https://ilinkai.weixin.qq.com/ilink/bot/sendmessage"
        );
        for header in ["AuthorizationType", "X-WECHAT-UIN", "iLink-App-Id"] {
            assert!(
                requests[0].headers.iter().any(|(name, _)| name == header),
                "missing {header}"
            );
        }
        let body = body_of(&requests[0]);
        assert_eq!(body["msg"]["to_user_id"], "peer1");
        assert_eq!(body["msg"]["message_type"], MSG_TYPE_BOT);
        assert_eq!(body["msg"]["item_list"][0]["type"], ITEM_TEXT);
        assert_eq!(body["msg"]["item_list"][0]["text_item"]["text"], "hi");
    }

    #[tokio::test]
    async fn a_cached_context_token_is_echoed() {
        let (adapter, mock) = build(vec![ok()]);
        adapter.remember_context_token("peer1", "ctx-1");
        adapter.send("peer1", "hi", None, None).await;
        assert_eq!(
            body_of(&mock.requests()[0])["msg"]["context_token"],
            "ctx-1"
        );
    }

    #[tokio::test]
    async fn long_text_is_chunked() {
        let content = "x".repeat(WEIXIN_MAX_MESSAGE_LENGTH + 10);
        let (adapter, mock) = build(vec![ok(), ok()]);
        let result = adapter.send("peer1", &content, None, None).await;
        assert!(result.success);
        assert_eq!(result.continuation_message_ids.len(), 2);
        for request in mock.requests() {
            let text = body_of(&request)["msg"]["item_list"][0]["text_item"]["text"]
                .as_str()
                .unwrap()
                .to_string();
            assert!(text.chars().count() <= WEIXIN_MAX_MESSAGE_LENGTH);
        }
    }

    #[tokio::test]
    async fn a_stale_session_is_retried_without_the_token() {
        let (adapter, mock) = build(vec![
            HttpResponse {
                status: 200,
                body: json!({"ret": -14, "errcode": 0, "errmsg": "session expired"}).to_string(),
                retry_after: None,
            },
            ok(),
        ]);
        adapter.remember_context_token("peer1", "ctx-old");
        let result = adapter.send("peer1", "hi", None, None).await;
        assert!(result.success);
        let requests = mock.requests();
        assert_eq!(requests.len(), 2);
        assert!(body_of(&requests[0])["msg"].get("context_token").is_some());
        assert!(body_of(&requests[1])["msg"].get("context_token").is_none());
    }

    #[tokio::test]
    async fn blank_inputs_make_no_request() {
        let (adapter, mock) = build(vec![]);
        assert!(!adapter.send("", "hi", None, None).await.success);
        assert!(!adapter.send("peer", " ", None, None).await.success);
        assert!(mock.requests().is_empty());
    }

    #[tokio::test]
    async fn poll_parses_text_messages_and_returns_the_cursor() {
        let (adapter, mock) = build(vec![HttpResponse {
            status: 200,
            body: json!({
                "ret": 0,
                "get_updates_buf": "cursor-2",
                "msgs": [
                    {"from_user_id": "peer1", "msg_id": "m1", "create_time": 1700000000,
                     "context_token": "ctx-9",
                     "item_list": [{"type": 1, "text_item": {"text": "hello"}}]},
                    {"from_user_id": "peer2", "msg_id": "m2", "room_id": "room-1",
                     "item_list": [{"type": 1, "text_item": {"text": "group hi"}}]},
                    {"from_user_id": "peer3", "msg_id": "m3", "item_list": []}
                ]
            })
            .to_string(),
            retry_after: None,
        }]);
        let (events, cursor) = adapter.poll_once(Some("cursor-1")).await.unwrap();
        assert_eq!(cursor, "cursor-2");
        assert_eq!(events.len(), 2);
        assert_eq!(events[0].text, "hello");
        assert_eq!(events[0].message_id.as_deref(), Some("m1"));
        assert_eq!(events[1].source.chat_type, "group");
        assert_eq!(events[1].source.chat_id, "room-1");
        assert_eq!(body_of(&mock.requests()[0])["get_updates_buf"], "cursor-1");
        assert_eq!(adapter.context_token("peer1").as_deref(), Some("ctx-9"));
    }

    #[tokio::test]
    async fn connect_requires_credentials() {
        let (mut adapter, _) = build(vec![]);
        assert!(adapter.connect(false).await);
        let (mut empty, _) = build(vec![]);
        empty.token.clear();
        assert!(!empty.connect(false).await);
    }

    // ----- outbound media (`_send_file`, adapter.py:1142-1190) -----

    #[tokio::test]
    async fn supports_media_reports_uploads() {
        let (adapter, _) = build(vec![]);
        assert!(adapter.supports_media());
    }

    #[tokio::test]
    async fn a_url_source_is_refused_without_a_request() {
        let (adapter, mock) = build(vec![]);
        let media = OutboundMedia::image_url("https://example.test/cat.png").with_caption("hi");
        let result = adapter.send_media("peer1", &media, None).await;
        assert!(!result.success);
        assert_eq!(result.error_kind.as_deref(), Some("bad_format"));
        let error = result.error.unwrap_or_default();
        assert!(error.contains("MediaSource::Bytes"), "{error}");
        assert!(error.contains("https://example.test/cat.png"), "{error}");
        assert!(
            mock.requests().is_empty(),
            "a refusal must not hit the wire"
        );
    }

    #[tokio::test]
    async fn empty_bytes_and_a_blank_recipient_make_no_request() {
        let (adapter, mock) = build(vec![]);
        let empty = OutboundMedia::new(MediaKind::Image, MediaSource::Bytes(Vec::new()));
        let result = adapter.send_media("peer1", &empty, None).await;
        assert!(!result.success);
        assert_eq!(result.error_kind.as_deref(), Some("bad_format"));
        assert!(
            result
                .error
                .as_deref()
                .unwrap_or_default()
                .contains("empty")
        );

        let result = adapter.send_media("  ", &image_media(), None).await;
        assert!(!result.success);
        assert_eq!(result.error_kind.as_deref(), Some("bad_format"));
        assert!(mock.requests().is_empty());
    }

    #[test]
    fn media_size_caps_come_from_the_wire_integers() {
        assert!(media_size_error(0).is_some());
        assert!(media_size_error(1).is_none());
        assert!(media_size_error(WEIXIN_MEDIA_MAX_BYTES).is_none());
        let (kind, message) = media_size_error(WEIXIN_MEDIA_MAX_BYTES + 1).expect("over the cap");
        assert_eq!(kind, "too_long");
        assert!(message.contains("32-bit"), "{message}");
        assert_eq!(WEIXIN_MEDIA_MAX_BYTES, i32::MAX as usize);
    }

    #[test]
    fn pkcs7_padding_and_the_filesize_formula_match_the_reference() {
        assert_eq!(pkcs7_pad(b""), vec![16u8; 16]);
        assert_eq!(pkcs7_pad(b"hello"), {
            let mut expected = b"hello".to_vec();
            expected.extend([11u8; 11]);
            expected
        });
        // An already-aligned payload gains a whole pad block (adapter.py:132-134).
        let aligned = pkcs7_pad(&[0xab; 16]);
        assert_eq!(aligned.len(), 32);
        assert_eq!(&aligned[16..], &[16u8; 16]);

        // `((rawsize + 16) // 16) * 16` (adapter.py:1150) is the padded length, always.
        for rawsize in [1usize, 5, 15, 16, 17, 31, 32, 1000] {
            assert_eq!(padded_size(rawsize), pkcs7_pad(&vec![0u8; rawsize]).len());
        }
    }

    #[test]
    fn aes128_ecb_matches_the_nist_vector_and_pads_with_pkcs7() {
        // Key from NIST SP 800-38A F.1.1; the first ciphertext block is that published vector, and
        // the second is the PKCS#7 pad block. Cross-checked with `openssl enc -aes-128-ecb -nosalt`.
        let key: [u8; AES_KEY_BYTES] = [
            0x2b, 0x7e, 0x15, 0x16, 0x28, 0xae, 0xd2, 0xa6, 0xab, 0xf7, 0x15, 0x88, 0x09, 0xcf,
            0x4f, 0x3c,
        ];
        let plaintext = hex_decode("6bc1bee22e409f96e93d7e117393172a");
        let ciphertext = aes128_ecb_encrypt(&plaintext, &key).expect("encrypt");
        assert_eq!(
            hex_encode(&ciphertext),
            "3ad77bb40d7a3660a89ecaf32466ef97a254be88e037ddd9d79fb6411c3f9df8"
        );
        // 16 bytes in → 32 bytes out: PKCS#7 added a full block.
        assert_eq!(ciphertext.len(), padded_size(plaintext.len()));

        // Five bytes → one block, and the vector matches the same tool.
        let key: [u8; AES_KEY_BYTES] = [
            0x00, 0x01, 0x02, 0x03, 0x04, 0x05, 0x06, 0x07, 0x08, 0x09, 0x0a, 0x0b, 0x0c, 0x0d,
            0x0e, 0x0f,
        ];
        let ciphertext = aes128_ecb_encrypt(b"hello", &key).expect("encrypt");
        assert_eq!(ciphertext.len(), 16);
        assert_eq!(hex_encode(&ciphertext), "5d8749e2af7531b2bf6661e9e5daf012");
    }

    #[test]
    fn md5_matches_the_rfc_1321_vectors() {
        assert_eq!(md5_hex(b""), "d41d8cd98f00b204e9800998ecf8427e");
        assert_eq!(md5_hex(b"abc"), "900150983cd24fb0d6963f7d28e17f72");
        assert_eq!(
            md5_hex(b"The quick brown fox jumps over the lazy dog"),
            "9e107d9d372bb6826bd81d3542a419d6"
        );
    }

    #[test]
    fn random_keys_are_hex_and_the_right_length() {
        let key = random_aes_key().expect("entropy");
        assert_eq!(key.len(), AES_KEY_BYTES);
        let filekey = random_hex(FILEKEY_BYTES).expect("entropy");
        assert_eq!(filekey.len(), FILEKEY_BYTES * 2);
        assert!(filekey.chars().all(|c| c.is_ascii_hexdigit()));
        assert_ne!(random_hex(FILEKEY_BYTES).expect("entropy"), filekey);
    }

    #[test]
    fn percent_encoding_matches_python_quote_with_an_empty_safe_set() {
        assert_eq!(percent_encode("abc-_.~"), "abc-_.~");
        assert_eq!(percent_encode("a+b/c=="), "a%2Bb%2Fc%3D%3D");
        assert_eq!(percent_encode("a b&c"), "a%20b%26c");
    }

    #[tokio::test]
    async fn getuploadurl_posts_the_reference_fields() {
        let (adapter, mock) = build(happy_responses());
        let payload = payload();
        assert!(
            adapter
                .send_media("peer1", &image_media(), None)
                .await
                .success
        );

        let requests = mock.requests();
        assert_eq!(
            requests[0].url,
            "https://ilinkai.weixin.qq.com/ilink/bot/getuploadurl"
        );
        assert_eq!(requests[0].method, HttpMethod::Post);
        for header in [
            "AuthorizationType",
            "X-WECHAT-UIN",
            "iLink-App-Id",
            "iLink-App-ClientVersion",
            "Authorization",
        ] {
            assert!(
                requests[0].headers.iter().any(|(name, _)| name == header),
                "missing {header}"
            );
        }
        let body = body_of(&requests[0]);
        // Field for field, adapter.py:333-340.
        assert_eq!(body["media_type"], MEDIA_TYPE_IMAGE);
        assert_eq!(body["to_user_id"], "peer1");
        assert_eq!(body["rawsize"], payload.len());
        assert_eq!(body["rawfilemd5"], md5_hex(&payload));
        assert_eq!(body["filesize"], padded_size(payload.len()));
        assert_eq!(body["no_need_thumb"], true);
        let filekey = body["filekey"].as_str().expect("filekey");
        assert_eq!(filekey.len(), FILEKEY_BYTES * 2, "secrets.token_hex(16)");
        assert!(filekey.chars().all(|c| c.is_ascii_hexdigit()));
        let aeskey = body["aeskey"].as_str().expect("aeskey");
        assert_eq!(aeskey.len(), AES_KEY_BYTES * 2);
        assert_eq!(hex_decode(aeskey).len(), AES_KEY_BYTES);
    }

    #[tokio::test]
    async fn cdn_upload_posts_the_ciphertext_with_only_the_content_type() {
        let (adapter, mock) = build(happy_responses());
        let payload = payload();
        assert!(
            adapter
                .send_media("peer1", &image_media(), None)
                .await
                .success
        );

        let requests = mock.requests();
        let upload = &requests[1];
        assert_eq!(upload.url, "https://cdn.test/upload");
        assert_eq!(upload.method, HttpMethod::Post);
        assert_eq!(
            upload.headers,
            vec![(
                "Content-Type".to_string(),
                "application/octet-stream".to_string()
            )],
            "the CDN POST carries no bot token, exactly like adapter.py:345"
        );
        // The body is the AES-128-ECB(+PKCS#7) encryption with the key from getuploadurl.
        let key: [u8; AES_KEY_BYTES] =
            hex_decode(body_of(&requests[0])["aeskey"].as_str().expect("aeskey"))
                .try_into()
                .expect("16-byte key");
        let expected = aes128_ecb_encrypt(&payload, &key).expect("encrypt");
        assert_eq!(upload.body.as_deref(), Some(expected.as_slice()));
        assert_eq!(upload.body.as_ref().map(Vec::len), Some(padded_size(16)));
    }

    #[tokio::test]
    async fn the_cdn_url_is_built_from_upload_param_when_no_full_url_is_given() {
        let (adapter, mock) = build(vec![
            json_ok(json!({"ret": 0, "upload_param": "a b+c/d="})),
            cdn_ok("token-2"),
            ok(),
        ]);
        assert!(
            adapter
                .send_media("peer1", &image_media(), None)
                .await
                .success
        );
        let requests = mock.requests();
        let filekey = body_of(&requests[0])["filekey"]
            .as_str()
            .expect("filekey")
            .to_string();
        assert_eq!(
            requests[1].url,
            format!(
                "{WEIXIN_CDN_BASE_URL}/upload?encrypted_query_param=a%20b%2Bc%2Fd%3D&filekey={filekey}"
            )
        );
    }

    #[tokio::test]
    async fn the_media_message_carries_the_reference_media_fields() {
        let (adapter, mock) = build(happy_responses());
        adapter.remember_context_token("peer1", "ctx-1");
        let payload = payload();
        let result = adapter
            .send_media("peer1", &image_media(), Some("ignored"))
            .await;
        assert!(result.success, "{result:?}");
        assert_eq!(result.error_kind, None);

        let requests = mock.requests();
        assert_eq!(
            requests[2].url,
            "https://ilinkai.weixin.qq.com/ilink/bot/sendmessage"
        );
        let msg = body_of(&requests[2])["msg"].clone();
        assert_eq!(msg["from_user_id"], "");
        assert_eq!(msg["to_user_id"], "peer1");
        assert_eq!(msg["message_type"], MSG_TYPE_BOT);
        assert_eq!(msg["message_state"], MSG_STATE_FINISH);
        assert_eq!(msg["context_token"], "ctx-1");
        assert!(
            msg["client_id"]
                .as_str()
                .unwrap_or_default()
                .starts_with("vikram-weixin-")
        );
        let item = &msg["item_list"][0];
        assert_eq!(item["type"], ITEM_IMAGE);
        let media = &item["image_item"]["media"];
        assert_eq!(media["encrypt_query_param"], "enc-token-1");
        assert_eq!(media["encrypt_type"], 1);
        let aeskey_hex = body_of(&requests[0])["aeskey"]
            .as_str()
            .expect("aeskey")
            .to_string();
        assert_eq!(
            media["aes_key"],
            Value::String(base64_encode(aeskey_hex.as_bytes())),
            "aes_key is base64(hex string), not base64(raw bytes) (adapter.py:1160-1163)"
        );
        assert_eq!(
            item["image_item"]["mid_size"],
            padded_size(payload.len()),
            "mid_size is the ciphertext size"
        );
        assert_eq!(
            result.message_id,
            msg["client_id"].as_str().map(str::to_owned)
        );
        // One media message, no caption: the id list holds exactly that send.
        assert_eq!(result.continuation_message_ids.len(), 1);
        assert_eq!(
            result.continuation_message_ids.first().map(String::as_str),
            result.message_id.as_deref()
        );
    }

    #[tokio::test]
    async fn document_and_video_and_audio_use_the_reference_item_shapes() {
        // Documents ship as file_item with a *string* length (adapter.py:662).
        let (adapter, mock) = build(vec![upload_ticket(), cdn_ok("tok-file"), ok()]);
        let media = OutboundMedia::new(MediaKind::Document, MediaSource::Bytes(payload()))
            .with_mime("application/pdf")
            .with_filename("report.pdf");
        assert!(adapter.send_media("peer1", &media, None).await.success);
        let requests = mock.requests();
        assert_eq!(body_of(&requests[0])["media_type"], MEDIA_TYPE_FILE);
        let item = &body_of(&requests[2])["msg"]["item_list"][0];
        assert_eq!(item["type"], ITEM_FILE);
        assert_eq!(item["file_item"]["file_name"], "report.pdf");
        assert_eq!(item["file_item"]["len"], payload().len().to_string());
        assert!(item["file_item"]["len"].is_string());

        // Video carries video_size, play_length and video_md5 (adapter.py:664-665).
        let (adapter, mock) = build(vec![upload_ticket(), cdn_ok("tok-video"), ok()]);
        let media = OutboundMedia::new(MediaKind::Video, MediaSource::Bytes(payload()))
            .with_filename("clip.mp4");
        assert!(adapter.send_media("peer1", &media, None).await.success);
        let requests = mock.requests();
        assert_eq!(body_of(&requests[0])["media_type"], MEDIA_TYPE_VIDEO);
        let item = &body_of(&requests[2])["msg"]["item_list"][0];
        assert_eq!(item["type"], ITEM_VIDEO);
        assert_eq!(item["video_item"]["video_size"], padded_size(16));
        assert_eq!(item["video_item"]["play_length"], 0);
        assert_eq!(item["video_item"]["video_md5"], md5_hex(&payload()));

        // Audio is deliberately a file attachment (adapter.py:1128-1130).
        let (adapter, mock) = build(vec![upload_ticket(), cdn_ok("tok-audio"), ok()]);
        let media = OutboundMedia::new(MediaKind::Audio, MediaSource::Bytes(payload()))
            .with_filename("voice.mp3");
        assert!(adapter.send_media("peer1", &media, None).await.success);
        let requests = mock.requests();
        assert_eq!(body_of(&requests[0])["media_type"], MEDIA_TYPE_FILE);
        let item = &body_of(&requests[2])["msg"]["item_list"][0];
        assert_eq!(item["type"], ITEM_FILE);
        assert_eq!(item["file_item"]["file_name"], "voice.mp3");
    }

    #[tokio::test]
    async fn a_missing_filename_falls_back_per_kind() {
        let (adapter, mock) = build(vec![upload_ticket(), cdn_ok("tok"), ok()]);
        let media = OutboundMedia::new(MediaKind::Image, MediaSource::Bytes(payload()));
        assert!(adapter.send_media("peer1", &media, None).await.success);
        let requests = mock.requests();
        let item = &body_of(&requests[2])["msg"]["item_list"][0];
        assert_eq!(item["image_item"].get("file_name"), None);
        assert_eq!(default_filename(MediaKind::Image), "image.jpg");
        assert_eq!(default_filename(MediaKind::Document), "document.bin");
    }

    #[tokio::test]
    async fn a_caption_is_sent_as_a_preceding_text_message() {
        let (adapter, mock) = build(vec![upload_ticket(), cdn_ok("tok"), ok(), ok()]);
        let media = image_media().with_caption("look at this");
        let result = adapter.send_media("peer1", &media, None).await;
        assert!(result.success);
        let requests = mock.requests();
        assert_eq!(requests.len(), 4);
        let caption = body_of(&requests[2])["msg"].clone();
        assert_eq!(caption["item_list"][0]["type"], ITEM_TEXT);
        assert_eq!(caption["item_list"][0]["text_item"]["text"], "look at this");
        let media_msg = body_of(&requests[3])["msg"].clone();
        assert_eq!(media_msg["item_list"][0]["type"], ITEM_IMAGE);
        // Both ids in send order, media last (adapter.py:1166-1190).
        assert_eq!(result.continuation_message_ids.len(), 2);
        assert_eq!(
            result.continuation_message_ids.last().map(String::as_str),
            result.message_id.as_deref()
        );
        assert_eq!(
            result.continuation_message_ids[0],
            caption["client_id"].as_str().unwrap_or_default()
        );
        assert_eq!(
            result.message_id.as_deref(),
            media_msg["client_id"].as_str()
        );
    }

    #[tokio::test]
    async fn a_blank_caption_is_not_sent() {
        let (adapter, mock) = build(vec![upload_ticket(), cdn_ok("tok"), ok()]);
        let media = image_media().with_caption("   \n ");
        assert!(adapter.send_media("peer1", &media, None).await.success);
        assert_eq!(mock.requests().len(), 3);
        assert_eq!(
            body_of(&mock.requests()[2])["msg"]["item_list"][0]["type"],
            ITEM_IMAGE
        );
    }

    #[tokio::test]
    async fn an_upload_url_failure_stops_before_the_cdn() {
        for (scripted, expected_kind) in [
            (status(500, "boom"), "transient"),
            (status(429, "slow down"), "rate_limited"),
            (status(403, "nope"), "forbidden"),
            (status(400, "bad"), "bad_format"),
            (status(404, "gone"), "not_found"),
            (status(413, "big"), "too_long"),
            (status(418, "teapot"), "unknown"),
        ] {
            let (adapter, mock) = build(vec![scripted]);
            let result = adapter.send_media("peer1", &image_media(), None).await;
            assert!(!result.success);
            assert_eq!(result.error_kind.as_deref(), Some(expected_kind));
            assert_eq!(
                result
                    .raw_response
                    .as_ref()
                    .and_then(|raw| raw["phase"].as_str()),
                Some("upload_url")
            );
            assert_eq!(mock.requests().len(), 1, "the CDN must never be touched");
        }
    }

    #[tokio::test]
    async fn an_upload_ticket_without_a_cdn_target_stops_before_the_cdn() {
        let (adapter, mock) = build(vec![json_ok(json!({"ret": 0}))]);
        let result = adapter.send_media("peer1", &image_media(), None).await;
        assert!(!result.success);
        assert!(
            result
                .error
                .as_deref()
                .unwrap_or_default()
                .contains("neither upload_param nor upload_full_url")
        );
        assert_eq!(
            result
                .raw_response
                .as_ref()
                .and_then(|raw| raw["phase"].as_str()),
            Some("upload_url")
        );
        assert_eq!(mock.requests().len(), 1);
    }

    #[tokio::test]
    async fn a_cdn_status_failure_stops_before_the_message() {
        let (adapter, mock) = build(vec![upload_ticket(), status(403, "denied")]);
        let result = adapter.send_media("peer1", &image_media(), None).await;
        assert!(!result.success);
        assert_eq!(result.error_kind.as_deref(), Some("forbidden"));
        assert!(!result.retryable);
        assert_eq!(
            result
                .raw_response
                .as_ref()
                .and_then(|raw| raw["phase"].as_str()),
            Some("cdn_upload")
        );
        assert_eq!(
            mock.requests().len(),
            2,
            "no sendmessage after a CDN failure"
        );
    }

    #[tokio::test]
    async fn a_cdn_response_without_a_token_stops_before_the_message() {
        let (adapter, mock) = build(vec![
            upload_ticket(),
            status(200, "<html>not a token</html>"),
        ]);
        let result = adapter.send_media("peer1", &image_media(), None).await;
        assert!(!result.success);
        assert!(
            result
                .error
                .as_deref()
                .unwrap_or_default()
                .contains("x-encrypted-param"),
            "{:?}",
            result.error
        );
        assert_eq!(
            result
                .raw_response
                .as_ref()
                .and_then(|raw| raw["phase"].as_str()),
            Some("cdn_upload")
        );
        assert_eq!(mock.requests().len(), 2);
    }

    #[tokio::test]
    async fn a_media_message_error_surfaces_the_phase() {
        let (adapter, mock) = build(vec![
            upload_ticket(),
            cdn_ok("tok"),
            json_ok(json!({"ret": 0, "errcode": -2, "errmsg": "rate limited"})),
        ]);
        let result = adapter.send_media("peer1", &image_media(), None).await;
        assert!(!result.success);
        assert_eq!(result.error_kind.as_deref(), Some("rate_limited"));
        assert!(result.retryable);
        assert_eq!(
            result
                .raw_response
                .as_ref()
                .and_then(|raw| raw["phase"].as_str()),
            Some("media_message")
        );
        assert_eq!(mock.requests().len(), 3);
    }

    #[tokio::test]
    async fn a_not_ready_session_is_forbidden_and_not_retried() {
        let (adapter, mock) = build(vec![
            upload_ticket(),
            cdn_ok("tok"),
            json_ok(json!({"ret": -2, "errcode": -2, "errmsg": "prepare failed"})),
        ]);
        let result = adapter.send_media("peer1", &image_media(), None).await;
        assert!(!result.success);
        assert_eq!(result.error_kind.as_deref(), Some("forbidden"));
        assert!(!result.retryable);
        assert_eq!(mock.requests().len(), 3);
    }

    #[tokio::test]
    async fn a_stale_session_is_retried_once_without_the_context_token() {
        let (adapter, mock) = build(vec![
            upload_ticket(),
            cdn_ok("tok"),
            json_ok(json!({"ret": -14, "errcode": 0, "errmsg": "session expired"})),
            ok(),
        ]);
        adapter.remember_context_token("peer1", "ctx-old");
        let result = adapter.send_media("peer1", &image_media(), None).await;
        assert!(result.success, "{result:?}");
        let requests = mock.requests();
        assert_eq!(requests.len(), 4);
        assert_eq!(body_of(&requests[2])["msg"]["context_token"], "ctx-old");
        assert!(
            body_of(&requests[3])["msg"].get("context_token").is_none(),
            "the retry drops the stale token (adapter.py:1181-1185)"
        );
        assert!(
            adapter.context_token("peer1").is_none(),
            "the stale token is forgotten for later sends"
        );
    }

    #[tokio::test]
    async fn a_transport_error_is_retryable_and_stops_the_chain() {
        struct BrokenTransport;
        #[async_trait]
        impl Transport for BrokenTransport {
            async fn execute(&self, _request: HttpRequest) -> Result<HttpResponse, TransportError> {
                Err(TransportError::Request("connection reset".into()))
            }
        }
        let adapter = WeixinAdapter::new(
            Arc::new(BrokenTransport) as SharedTransport,
            "https://ilinkai.weixin.qq.com",
            "acct",
            "tok",
        );
        let result = adapter.send_media("peer1", &image_media(), None).await;
        assert!(!result.success);
        assert_eq!(result.error_kind.as_deref(), Some("transient"));
        assert!(result.retryable);
        assert_eq!(
            result
                .raw_response
                .as_ref()
                .and_then(|raw| raw["phase"].as_str()),
            Some("upload_url")
        );
    }

    #[test]
    fn status_mapping_covers_the_reference_statuses() {
        assert_eq!(classify_status(400), ("bad_format", false));
        assert_eq!(classify_status(403), ("forbidden", false));
        assert_eq!(classify_status(404), ("not_found", false));
        assert_eq!(classify_status(413), ("too_long", false));
        assert_eq!(classify_status(429), ("rate_limited", true));
        assert_eq!(classify_status(500), ("transient", true));
        assert_eq!(classify_status(503), ("transient", true));
        assert_eq!(classify_status(418), ("unknown", false));
        assert_eq!(classify_status(401), ("unknown", false));
    }

    #[test]
    fn media_shapes_follow_the_reference_mime_mapping() {
        assert_eq!(
            media_shape(MediaKind::Image),
            (MEDIA_TYPE_IMAGE, ITEM_IMAGE)
        );
        assert_eq!(
            media_shape(MediaKind::Video),
            (MEDIA_TYPE_VIDEO, ITEM_VIDEO)
        );
        assert_eq!(media_shape(MediaKind::Audio), (MEDIA_TYPE_FILE, ITEM_FILE));
        assert_eq!(
            media_shape(MediaKind::Document),
            (MEDIA_TYPE_FILE, ITEM_FILE)
        );
    }

    #[test]
    fn the_upload_ticket_prefers_the_direct_cdn_url() {
        let ticket = json!({
            "upload_full_url": "https://cdn.test/direct",
            "upload_param": "ignored",
        });
        assert_eq!(
            upload_url_from(WEIXIN_CDN_BASE_URL, &ticket, "fk"),
            Ok("https://cdn.test/direct".to_string())
        );
        let ticket = json!({"upload_full_url": "", "upload_param": "p"});
        assert_eq!(
            upload_url_from("https://cdn.test/c2c/", &ticket, "fk"),
            Ok("https://cdn.test/c2c/upload?encrypted_query_param=p&filekey=fk".to_string())
        );
        assert!(upload_url_from(WEIXIN_CDN_BASE_URL, &json!({}), "fk").is_err());
    }
}
