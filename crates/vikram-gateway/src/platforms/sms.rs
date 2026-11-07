//! SMS adapter — Twilio Programmable Messaging.
//!
//! Outbound is one REST call per chunk: `POST {api_base}/2010-04-01/Accounts/{sid}/Messages.json`
//! with `Authorization: Basic base64(sid:token)` and an `application/x-www-form-urlencoded` body of
//! `From` / `To` / `Body`. Inbound is a Twilio webhook: the host owns the HTTP listener and hands the
//! raw URL, the form-encoded POST parameters and the `X-Twilio-Signature` header to
//! [`SmsAdapter::verify_webhook`] and [`SmsAdapter::parse_webhook`]. This adapter never binds a port
//! and never renders TwiML, exactly like the reference implementation (replies always go out over
//! the REST API).
//!
//! Port of `research/hermes-platforms/sms/adapter.py` (the wire half). Fidelity notes:
//!
//! - `send` chunks at [`SMS_MAX_MESSAGE_LENGTH`] (1600 code points, ~10 SMS segments) with the
//!   shared fence-aware [`truncate_message`] and delivers the chunks sequentially; the reported
//!   `message_id` is the last chunk's SID and every SID rides along in
//!   `continuation_message_ids`, in send order.
//! - Failure taxonomy: `429` → `rate_limited` (retryable, honouring `Retry-After`),
//!   `401`/`403` → `forbidden`, `404` → `not_found`, `400` → `bad_format` (Twilio's
//!   `{"code": 21610, "message": …}` error body is flattened into the error text),
//!   `5xx` and transport errors → `transient` (retryable).
//! - Signature verification is Twilio's scheme: `base64(HMAC-SHA1(auth_token, url + Σ(key+value)
//!   sorted by key))`, compared in constant time. An empty auth token fails closed, because an
//!   empty HMAC key is public.
//! - Both Base64 and SHA-1 are implemented in this file: the crate forbids new dependencies and
//!   neither is reachable through `crate::pairing::sha256`. SHA-1 follows FIPS 180-1 and is checked
//!   against the published vectors.
//!
//! Deliberately not ported (needs runtime state or a listener the host owns): the aiohttp webhook
//! server, its 64 KiB body cap and TwiML 200/400/403 responses, `SMS_WEBHOOK_URL` /
//! `SMS_INSECURE_NO_SIGNATURE` gating, the `SMS_ALLOWED_USERS` allowlist and `SMS_HOME_CHANNEL` cron
//! routing (access control lives in `crate::access`), the `redact_phone` logging helpers, markdown
//! stripping for the standalone sender, and the default-port URL variant retried during signature
//! validation (callers must pass the exact URL Twilio was configured with).

use async_trait::async_trait;
use serde_json::Value;

use crate::event::{MessageEvent, MessageType, Platform, SessionSource, platform_names};
use crate::helpers::{codepoint_len, truncate_message};
use crate::platform::{ChatInfo, PlatformAdapter, SendResult};
use crate::transport::{HttpRequest, SharedTransport};

/// Twilio's REST origin; [`SmsAdapter::with_api_base`] overrides it for mirrors and tests.
pub const TWILIO_API_BASE: &str = "https://api.twilio.com";

/// Twilio's API version path segment, part of every account-scoped URL.
const API_VERSION: &str = "2010-04-01";

/// Outbound cap: 1600 code points, roughly ten SMS segments (hermes `MAX_SMS_LENGTH`).
pub const SMS_MAX_MESSAGE_LENGTH: usize = 1600;

/// Twilio Programmable Messaging adapter.
///
/// Stateless with respect to the conversation: a chat id *is* the peer's E.164 phone number, and
/// every reply leaves from the one configured number.
pub struct SmsAdapter {
    transport: SharedTransport,
    account_sid: String,
    auth_token: String,
    from_number: String,
    api_base: String,
}

impl SmsAdapter {
    /// Build an adapter for one Twilio account, authenticating with `account_sid` + `auth_token` and
    /// sending from `from_number` (E.164, e.g. `+15551234567`).
    pub fn new(
        transport: SharedTransport,
        account_sid: impl Into<String>,
        auth_token: impl Into<String>,
        from_number: impl Into<String>,
    ) -> Self {
        Self {
            transport,
            account_sid: account_sid.into(),
            auth_token: auth_token.into(),
            from_number: from_number.into(),
            api_base: "https://api.twilio.com".to_string(),
        }
    }

    /// Point the adapter at a mirror or a test double; a trailing `/` is dropped.
    pub fn with_api_base(mut self, base: impl Into<String>) -> Self {
        self.api_base = base.into().trim_end_matches('/').to_string();
        self
    }

    /// `{api_base}/2010-04-01/Accounts/{sid}.json` — the account resource `connect` probes.
    fn account_url(&self) -> String {
        format!(
            "{}/{API_VERSION}/Accounts/{}.json",
            self.api_base, self.account_sid
        )
    }

    /// `{api_base}/2010-04-01/Accounts/{sid}/Messages.json` — the create-a-message endpoint.
    fn messages_url(&self) -> String {
        format!(
            "{}/{API_VERSION}/Accounts/{}/Messages.json",
            self.api_base, self.account_sid
        )
    }

    /// The `Authorization` header value: HTTP Basic over `account_sid:auth_token`.
    fn auth_header(&self) -> String {
        let credentials = format!("{}:{}", self.account_sid, self.auth_token);
        format!("Basic {}", base64_encode(credentials.as_bytes()))
    }

    /// Verify the `X-Twilio-Signature` header of a webhook POST.
    ///
    /// Twilio signs `url + "".join(key + value for key sorted by key)` with HMAC-SHA1 keyed by the
    /// account auth token and sends the Base64 digest. `url` must be the exact public URL the
    /// webhook was configured with. The comparison is constant time, and an empty auth token always
    /// fails: an empty HMAC key is public, so verifying against it would accept anything.
    pub fn verify_webhook(&self, url: &str, params: &[(&str, String)], signature: &str) -> bool {
        if self.auth_token.trim().is_empty() {
            return false;
        }
        let data = signature_payload(url, params);
        let computed = base64_encode(&hmac_sha1(self.auth_token.as_bytes(), data.as_bytes()));
        constant_time_eq(computed.as_bytes(), signature.as_bytes())
    }

    /// Normalize one Twilio webhook POST into a [`MessageEvent`].
    ///
    /// Twilio posts form-encoded fields, so `params` is the decoded body in wire order. Behaviour:
    ///
    /// - no `Body` (status callbacks, delivery receipts) or a blank one → `Ok(None)`;
    /// - `Body` without a `From` → `Err`, because a reply could not be addressed;
    /// - `From` equal to the configured number → `Ok(None)`: that is our own outbound echo;
    /// - otherwise an event whose `chat_id` and `user_id` are the sender's number, whose
    ///   `message_id` is `MessageSid`, and whose text is `Body`.
    ///
    /// `platform_update_id` stays `None`: Twilio has no monotonically increasing update id, and a
    /// SID is not a number.
    pub fn parse_webhook(&self, params: &[(&str, String)]) -> Result<Option<MessageEvent>, String> {
        let Some(body) = param_value(params, "Body") else {
            return Ok(None);
        };
        let text = body.trim();
        if text.is_empty() {
            return Ok(None);
        }
        let from = param_value(params, "From")
            .map(str::trim)
            .unwrap_or_default();
        if from.is_empty() {
            return Err(
                "twilio webhook carried a Body without a From; cannot route the reply".to_string(),
            );
        }
        if from == self.from_number {
            // Echo prevention: Twilio reports our own outbound message back to the webhook.
            return Ok(None);
        }
        let message_id = param_value(params, "MessageSid")
            .map(str::trim)
            .filter(|sid| !sid.is_empty())
            .map(str::to_string);

        let mut source = SessionSource::new(sms_platform(), from.to_string());
        source.chat_name = Some(from.to_string());
        source.chat_type = "dm".to_string();
        source.user_id = Some(from.to_string());
        source.user_name = Some(from.to_string());
        source.message_id = message_id.clone();

        let mut event = MessageEvent::new(text, source);
        event.message_type = MessageType::Text;
        // hermes mirrors the author onto the event for per-message prompt builders.
        event.user_id = Some(from.to_string());
        event.user_name = Some(from.to_string());
        event.message_id = message_id;
        event.raw_message = Some(params_to_json(params));
        Ok(Some(event))
    }
}

#[async_trait]
impl PlatformAdapter for SmsAdapter {
    /// `GET /2010-04-01/Accounts/{sid}.json`; true only for a 2xx answer carrying a `sid`.
    ///
    /// `is_reconnect` needs no different handling: the adapter holds no server-side queue, so a
    /// reconnect is the same credential check as a first connect.
    async fn connect(&mut self, _is_reconnect: bool) -> bool {
        let request =
            HttpRequest::get(self.account_url()).header("Authorization", self.auth_header());
        let response = match self.transport.execute(request).await {
            Ok(response) => response,
            Err(_) => return false,
        };
        if !response.ok() {
            return false;
        }
        match response.json() {
            Ok(body) => body
                .get("sid")
                .and_then(Value::as_str)
                .is_some_and(|sid| !sid.trim().is_empty()),
            Err(_) => false,
        }
    }

    /// Nothing is held client-side (no HTTP session, no listener), so there is nothing to tear down.
    async fn disconnect(&mut self) {}

    /// Send `content` as one or more SMS messages, chunked at [`SMS_MAX_MESSAGE_LENGTH`].
    ///
    /// Every chunk is its own `Messages.json` call with `From`, `To` and `Body`; they go out
    /// sequentially so a mid-way failure leaves the delivered SIDs visible on the failure (plus the
    /// `partial_overflow` marker) and a retry only has to send the tail. Blank content is rejected
    /// as `bad_format` without a request: Twilio would answer `21602` (empty body) anyway.
    async fn send(
        &self,
        chat_id: &str,
        content: &str,
        _reply_to: Option<&str>,
        _metadata: Option<Value>,
    ) -> SendResult {
        if content.trim().is_empty() {
            return SendResult::failed("sms send skipped: content is empty", Some("bad_format"));
        }
        let chunks = truncate_message(content, SMS_MAX_MESSAGE_LENGTH, codepoint_len);
        let total = chunks.len();
        if total == 0 {
            return SendResult::failed("sms send produced no chunks", Some("bad_format"));
        }

        let mut delivered: Vec<String> = Vec::with_capacity(total);
        for chunk in &chunks {
            let request = HttpRequest::form(
                self.messages_url(),
                &[
                    ("From", self.from_number.clone()),
                    ("To", chat_id.to_string()),
                    ("Body", chunk.clone()),
                ],
            )
            .header("Authorization", self.auth_header());
            let response = match self.transport.execute(request).await {
                Ok(response) => response,
                Err(err) => {
                    return mark_partial(
                        SendResult::failed(
                            format!("twilio transport error: {err}"),
                            Some("transient"),
                        )
                        .retryable(None),
                        &delivered,
                        total,
                    );
                }
            };
            let body: Value = serde_json::from_str(&response.body).unwrap_or(Value::Null);
            if !response.ok() {
                return mark_partial(
                    failure_result(response.status, &body, &response.body, response.retry_after),
                    &delivered,
                    total,
                );
            }
            match body
                .get("sid")
                .and_then(Value::as_str)
                .map(str::trim)
                .filter(|sid| !sid.is_empty())
            {
                Some(sid) => delivered.push(sid.to_string()),
                None => {
                    return mark_partial(
                        SendResult::failed(
                            "twilio accepted the message but returned no sid",
                            Some("unknown"),
                        ),
                        &delivered,
                        total,
                    );
                }
            }
        }
        match delivered.last() {
            Some(last) => SendResult::ok_split(last.clone(), delivered),
            None => SendResult::failed("sms send delivered no chunks", Some("unknown")),
        }
    }

    /// `{name: chat_id, chat_type: "dm"}`.
    ///
    /// Twilio exposes no chat metadata: a conversation is a phone number and nothing else, so the
    /// number is its own display name and every conversation is a DM. No request is made, so this
    /// cannot fail or degrade.
    async fn get_chat_info(&self, chat_id: &str) -> ChatInfo {
        ChatInfo {
            name: chat_id.to_string(),
            chat_type: "dm".to_string(),
        }
    }

    /// `send` chunks natively, so the delivery router must not truncate first (hermes
    /// `splits_long_messages = True`).
    fn splits_long_messages(&self) -> bool {
        true
    }

    fn max_message_length(&self) -> usize {
        SMS_MAX_MESSAGE_LENGTH
    }

    /// No-op: Twilio's REST API has no typing indicator and SMS has no client-side affordance for
    /// one, so there is no request to make. Overridden explicitly so the no-op is a decision rather
    /// than an inherited default.
    async fn send_typing(&self, _chat_id: &str, _metadata: Option<Value>) {}
}

/// The exact string Twilio signs: the full URL followed by every POST parameter as `key + value`,
/// ordered by key (stable, so duplicate keys keep wire order).
fn signature_payload(url: &str, params: &[(&str, String)]) -> String {
    let mut pairs: Vec<(&str, &str)> = params
        .iter()
        .map(|(key, value)| (*key, value.as_str()))
        .collect();
    pairs.sort_by(|left, right| left.0.cmp(right.0));
    let mut data = String::with_capacity(
        url.len() + pairs.iter().map(|(k, v)| k.len() + v.len()).sum::<usize>(),
    );
    data.push_str(url);
    for (key, value) in pairs {
        data.push_str(key);
        data.push_str(value);
    }
    data
}

/// First occurrence of `key` (Twilio's `parse_qs(...)[0]` semantics).
fn param_value<'a>(params: &'a [(&str, String)], key: &str) -> Option<&'a str> {
    params
        .iter()
        .find(|(name, _)| *name == key)
        .map(|(_, value)| value.as_str())
}

/// The raw form payload as a JSON object, for `MessageEvent::raw_message`.
fn params_to_json(params: &[(&str, String)]) -> Value {
    let mut object = serde_json::Map::new();
    for (key, value) in params {
        object.insert((*key).to_string(), Value::String(value.clone()));
    }
    Value::Object(object)
}

/// Map one Twilio failure onto the shared `SendResult` taxonomy.
fn failure_result(
    status: u16,
    body: &Value,
    raw_body: &str,
    retry_after: Option<f64>,
) -> SendResult {
    let message = twilio_error_message(body, raw_body, status);
    match status {
        429 => SendResult::failed(message, Some("rate_limited")).retryable(retry_after),
        401 | 403 => SendResult::failed(message, Some("forbidden")),
        404 => SendResult::failed(message, Some("not_found")),
        500..=599 => SendResult::failed(message, Some("transient")).retryable(None),
        _ => SendResult::failed(message, Some("bad_format")),
    }
}

/// Twilio's error text: `{"code": 21610, "message": "..."}` flattened, else the shortened body, else
/// the status. Nothing here can fail.
fn twilio_error_message(body: &Value, raw_body: &str, status: u16) -> String {
    let code = body.get("code").and_then(Value::as_i64);
    let message = body
        .get("message")
        .and_then(Value::as_str)
        .map(str::trim)
        .filter(|message| !message.is_empty());
    match (code, message) {
        (Some(code), Some(message)) => format!("twilio error {code}: {message}"),
        (None, Some(message)) => message.to_string(),
        _ => {
            let raw = raw_body.trim();
            if raw.is_empty() {
                format!("twilio returned status {status}")
            } else {
                format!("twilio returned status {status}: {}", shortened(raw, 200))
            }
        }
    }
}

/// First `limit` characters plus an ellipsis, for error text.
fn shortened(text: &str, limit: usize) -> String {
    if text.chars().count() <= limit {
        return text.to_string();
    }
    let mut out: String = text.chars().take(limit).collect();
    out.push('…');
    out
}

/// Attach the ids of an already-delivered head (and the `partial_overflow` marker) to a failure, so
/// a retry only sends the tail.
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
fn sms_platform() -> Platform {
    match Platform::new(platform_names::SMS) {
        Some(platform) => platform,
        None => Platform::local(),
    }
}

/// Length-independent equality: every byte pair is compared, so the timing carries no information
/// about where two values first differ.
fn constant_time_eq(left: &[u8], right: &[u8]) -> bool {
    if left.len() != right.len() {
        return false;
    }
    let mut diff = 0u8;
    for (a, b) in left.iter().zip(right.iter()) {
        diff |= a ^ b;
    }
    diff == 0
}

/// Standard-alphabet Base64 with `=` padding (RFC 4648 §4), implemented here because the crate may
/// not add dependencies.
fn base64_encode(data: &[u8]) -> String {
    const ALPHABET: &[u8; 64] = b"ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz0123456789+/";
    let mut out = String::with_capacity(data.len().div_ceil(3) * 4);
    for chunk in data.chunks(3) {
        let first = chunk.first().copied().unwrap_or(0) as u32;
        let second = chunk.get(1).copied().unwrap_or(0) as u32;
        let third = chunk.get(2).copied().unwrap_or(0) as u32;
        let triple = (first << 16) | (second << 8) | third;
        out.push(ALPHABET[(triple >> 18) as usize & 0x3f] as char);
        out.push(ALPHABET[(triple >> 12) as usize & 0x3f] as char);
        if chunk.len() > 1 {
            out.push(ALPHABET[(triple >> 6) as usize & 0x3f] as char);
        } else {
            out.push('=');
        }
        if chunk.len() > 2 {
            out.push(ALPHABET[triple as usize & 0x3f] as char);
        } else {
            out.push('=');
        }
    }
    out
}

/// HMAC-SHA-1 (RFC 2104) over `message`, keyed by `key`.
fn hmac_sha1(key: &[u8], message: &[u8]) -> [u8; 20] {
    const BLOCK: usize = 64;
    let mut padded_key = [0u8; BLOCK];
    if key.len() > BLOCK {
        padded_key[..20].copy_from_slice(&sha1::digest(key));
    } else {
        padded_key[..key.len()].copy_from_slice(key);
    }
    let mut inner = Vec::with_capacity(BLOCK + message.len());
    let mut outer = Vec::with_capacity(BLOCK + 20);
    for byte in padded_key {
        inner.push(byte ^ 0x36);
    }
    for byte in padded_key {
        outer.push(byte ^ 0x5c);
    }
    inner.extend_from_slice(message);
    outer.extend_from_slice(&sha1::digest(&inner));
    sha1::digest(&outer)
}

/// SHA-1 (FIPS 180-1), implemented here because the crate may not add dependencies and the SHA-256
/// in `pairing.rs` cannot stand in for Twilio's scheme.
///
/// Only the webhook HMAC needs it, but it is a complete, vector-tested implementation.
mod sha1 {
    const H0: [u32; 5] = [
        0x6745_2301,
        0xefcd_ab89,
        0x98ba_dcfe,
        0x1032_5476,
        0xc3d2_e1f0,
    ];

    /// Raw 20-byte digest of `data`.
    pub(super) fn digest(data: &[u8]) -> [u8; 20] {
        let mut message = data.to_vec();
        let bit_len = (data.len() as u64).wrapping_mul(8);
        message.push(0x80);
        while message.len() % 64 != 56 {
            message.push(0);
        }
        message.extend_from_slice(&bit_len.to_be_bytes());

        let mut state = H0;
        for block in message.as_chunks::<64>().0 {
            let mut schedule = [0u32; 80];
            for (word, chunk) in schedule.iter_mut().zip(block.as_chunks::<4>().0) {
                *word = u32::from_be_bytes(*chunk);
            }
            for idx in 16..80 {
                schedule[idx] = (schedule[idx - 3]
                    ^ schedule[idx - 8]
                    ^ schedule[idx - 14]
                    ^ schedule[idx - 16])
                    .rotate_left(1);
            }

            let mut v = state;
            for (round, word) in schedule.iter().enumerate() {
                let (choose, constant) = match round {
                    0..=19 => ((v[1] & v[2]) | ((!v[1]) & v[3]), 0x5a82_7999),
                    20..=39 => (v[1] ^ v[2] ^ v[3], 0x6ed9_eba1),
                    40..=59 => ((v[1] & v[2]) | (v[1] & v[3]) | (v[2] & v[3]), 0x8f1b_bcdc),
                    _ => (v[1] ^ v[2] ^ v[3], 0xca62_c1d6),
                };
                let temp = v[0]
                    .rotate_left(5)
                    .wrapping_add(choose)
                    .wrapping_add(v[4])
                    .wrapping_add(constant)
                    .wrapping_add(*word);
                v[4] = v[3];
                v[3] = v[2];
                v[2] = v[1].rotate_left(30);
                v[1] = v[0];
                v[0] = temp;
            }
            for idx in 0..5 {
                state[idx] = state[idx].wrapping_add(v[idx]);
            }
        }

        let mut out = [0u8; 20];
        for (idx, word) in state.iter().enumerate() {
            out[idx * 4..idx * 4 + 4].copy_from_slice(&word.to_be_bytes());
        }
        out
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

        fn forms(&self) -> Vec<Vec<(String, String)>> {
            self.requests().iter().map(form_pairs).collect()
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

    const ACCOUNT_SID: &str = "AC1234567890";
    const AUTH_TOKEN: &str = "secret-token";
    const FROM: &str = "+15550000001";
    const API_BASE: &str = "https://mock.twilio.test";

    fn adapter(mock: &Arc<MockTransport>) -> SmsAdapter {
        SmsAdapter::new(mock.clone(), ACCOUNT_SID, AUTH_TOKEN, FROM).with_api_base(API_BASE)
    }

    fn response(status: u16, body: Value) -> Result<HttpResponse, TransportError> {
        Ok(HttpResponse {
            status,
            body: body.to_string(),
            retry_after: None,
        })
    }

    fn sent(sid: &str) -> Result<HttpResponse, TransportError> {
        response(
            201,
            serde_json::json!({
                "sid": sid,
                "status": "queued",
                "error_code": null,
                "error_message": null,
            }),
        )
    }

    fn form_pairs(request: &HttpRequest) -> Vec<(String, String)> {
        let body =
            String::from_utf8_lossy(request.body.as_deref().unwrap_or_default()).into_owned();
        body.split('&')
            .filter(|pair| !pair.is_empty())
            .map(|pair| match pair.split_once('=') {
                Some((key, value)) => (percent_decode(key), percent_decode(value)),
                None => (percent_decode(pair), String::new()),
            })
            .collect()
    }

    fn percent_decode(value: &str) -> String {
        let bytes = value.as_bytes();
        let mut out: Vec<u8> = Vec::with_capacity(bytes.len());
        let mut idx = 0;
        while idx < bytes.len() {
            if bytes[idx] == b'%'
                && idx + 2 < bytes.len()
                && let Ok(byte) = u8::from_str_radix(&value[idx + 1..idx + 3], 16)
            {
                out.push(byte);
                idx += 3;
                continue;
            }
            out.push(bytes[idx]);
            idx += 1;
        }
        String::from_utf8_lossy(&out).into_owned()
    }

    fn field(pairs: &[(String, String)], key: &str) -> Option<String> {
        pairs
            .iter()
            .find(|(name, _)| name == key)
            .map(|(_, value)| value.clone())
    }

    fn header(request: &HttpRequest, name: &str) -> Option<String> {
        request
            .headers
            .iter()
            .find(|(key, _)| key == name)
            .map(|(_, value)| value.clone())
    }

    fn hex(bytes: &[u8]) -> String {
        bytes.iter().map(|byte| format!("{byte:02x}")).collect()
    }

    fn long_content() -> String {
        (0..160)
            .map(|idx| format!("sms line {idx} with a few words to fill the message\n"))
            .collect()
    }

    #[test]
    fn base64_matches_known_vectors() {
        assert_eq!(base64_encode(b""), "");
        assert_eq!(base64_encode(b"f"), "Zg==");
        assert_eq!(base64_encode(b"fo"), "Zm8=");
        assert_eq!(base64_encode(b"foo"), "Zm9v");
        assert_eq!(base64_encode(b"foob"), "Zm9vYg==");
        assert_eq!(base64_encode(b"fooba"), "Zm9vYmE=");
        assert_eq!(base64_encode(b"foobar"), "Zm9vYmFy");
        assert_eq!(base64_encode(b"hello world"), "aGVsbG8gd29ybGQ=");
        // The account-credentials header value is Base64 over `sid:token`.
        assert_eq!(
            base64_encode(format!("{ACCOUNT_SID}:{AUTH_TOKEN}").as_bytes()),
            "QUMxMjM0NTY3ODkwOnNlY3JldC10b2tlbg=="
        );
    }

    #[test]
    fn sha1_matches_fips_vectors() {
        assert_eq!(
            hex(&sha1::digest(b"abc")),
            "a9993e364706816aba3e25717850c26c9cd0d89d"
        );
        assert_eq!(
            hex(&sha1::digest(b"")),
            "da39a3ee5e6b4b0d3255bfef95601890afd80709"
        );
        assert_eq!(
            hex(&sha1::digest(
                b"abcdbcdecdefdefgefghfghighijhijkijkljklmklmnlmnomnopnopq"
            )),
            "84983e441c3bd26ebaae4aa1f95129e5e54670f1"
        );
        // Multi-block input exercises padding and chaining.
        assert_eq!(hex(&sha1::digest(&vec![b'a'; 1024])).len(), 40);
    }

    #[test]
    fn hmac_sha1_matches_rfc2202_vectors() {
        // RFC 2202 test case 1.
        assert_eq!(
            hex(&hmac_sha1(&[0x0b; 20], b"Hi There")),
            "b617318655057264e28bc0b6fb378c8ef146be00"
        );
        // RFC 2202 test case 2.
        assert_eq!(
            hex(&hmac_sha1(b"Jefe", b"what do ya want for nothing?")),
            "effcdf6ae5eb2fa2d27416d5f184df9c259a7c79"
        );
    }

    #[tokio::test]
    async fn connect_probes_the_account_with_basic_auth() {
        let mock = MockTransport::new(vec![response(
            200,
            serde_json::json!({
                "sid": ACCOUNT_SID,
                "friendly_name": "VIKRAM",
                "status": "active",
            }),
        )]);
        let mut adapter = adapter(&mock);
        assert!(adapter.connect(false).await);

        let requests = mock.requests();
        assert_eq!(requests.len(), 1);
        assert_eq!(requests[0].method, HttpMethod::Get);
        assert_eq!(
            requests[0].url,
            "https://mock.twilio.test/2010-04-01/Accounts/AC1234567890.json"
        );
        assert_eq!(
            header(&requests[0], "Authorization").as_deref(),
            Some("Basic QUMxMjM0NTY3ODkwOnNlY3JldC10b2tlbg==")
        );
    }

    #[tokio::test]
    async fn with_api_base_trims_a_trailing_slash() {
        let mock = MockTransport::new(vec![sent("SM1")]);
        let adapter = SmsAdapter::new(mock.clone(), ACCOUNT_SID, AUTH_TOKEN, FROM)
            .with_api_base("https://mock.twilio.test/");
        assert!(adapter.send(FROM, "hi", None, None).await.success);
        assert_eq!(
            mock.requests()[0].url,
            "https://mock.twilio.test/2010-04-01/Accounts/AC1234567890/Messages.json"
        );
    }

    #[tokio::test]
    async fn connect_rejects_every_non_sid_answer() {
        // 401 from Twilio.
        let unauthorized = MockTransport::new(vec![response(
            401,
            serde_json::json!({"code": 20003, "message": "Authentication Error"}),
        )]);
        assert!(!adapter(&unauthorized).connect(true).await);

        // 200 without a sid.
        let idless =
            MockTransport::new(vec![response(200, serde_json::json!({"status": "active"}))]);
        assert!(!adapter(&idless).connect(false).await);

        // 200 with a blank sid.
        let blank = MockTransport::new(vec![response(200, serde_json::json!({"sid": "  "}))]);
        assert!(!adapter(&blank).connect(false).await);

        // A body that is not JSON at all.
        let garbage = MockTransport::new(vec![Ok(HttpResponse {
            status: 200,
            body: "<html>".to_string(),
            retry_after: None,
        })]);
        assert!(!adapter(&garbage).connect(false).await);

        // A transport failure.
        let down = MockTransport::new(vec![Err(TransportError::Request("dns".to_string()))]);
        assert!(!adapter(&down).connect(false).await);
    }

    #[tokio::test]
    async fn send_posts_the_twilio_form_and_returns_the_sid() {
        let mock = MockTransport::new(vec![sent("SM1111")]);
        let result = adapter(&mock)
            .send("+15550000002", "hello there", Some("ignored"), None)
            .await;

        assert!(result.success);
        assert_eq!(result.message_id.as_deref(), Some("SM1111"));
        assert_eq!(result.continuation_message_ids, vec!["SM1111"]);
        assert_eq!(result.error_kind, None);

        let requests = mock.requests();
        assert_eq!(requests.len(), 1);
        assert_eq!(requests[0].method, HttpMethod::Post);
        assert_eq!(
            requests[0].url,
            "https://mock.twilio.test/2010-04-01/Accounts/AC1234567890/Messages.json"
        );
        assert_eq!(
            header(&requests[0], "Content-Type").as_deref(),
            Some("application/x-www-form-urlencoded")
        );
        assert_eq!(
            header(&requests[0], "Authorization").as_deref(),
            Some("Basic QUMxMjM0NTY3ODkwOnNlY3JldC10b2tlbg==")
        );
        let pairs = &mock.forms()[0];
        assert_eq!(field(pairs, "From").as_deref(), Some(FROM));
        assert_eq!(field(pairs, "To").as_deref(), Some("+15550000002"));
        assert_eq!(field(pairs, "Body").as_deref(), Some("hello there"));
        // The `+` in an E.164 number must be percent-encoded on the wire.
        let raw = String::from_utf8_lossy(requests[0].body.as_deref().unwrap_or_default());
        assert!(raw.contains("To=%2B15550000002"), "{raw}");
    }

    #[tokio::test]
    async fn long_content_is_chunked_at_1600_and_sent_in_order() {
        let content = long_content();
        let expected = truncate_message(&content, SMS_MAX_MESSAGE_LENGTH, codepoint_len);
        assert!(expected.len() > 1, "test content must exceed one message");

        let mock = MockTransport::new(
            (0..expected.len())
                .map(|idx| sent(&format!("SM{idx}")))
                .collect(),
        );
        let result = adapter(&mock)
            .send("+15550000002", &content, None, None)
            .await;

        assert!(result.success);
        let requests = mock.requests();
        assert_eq!(requests.len(), expected.len());
        assert_eq!(
            result.message_id.as_deref(),
            Some(format!("SM{}", expected.len() - 1).as_str())
        );
        assert_eq!(result.continuation_message_ids.len(), expected.len());

        for (request, form) in requests.iter().zip(mock.forms()) {
            let body = field(&form, "Body").unwrap_or_default();
            assert!(
                body.chars().count() <= SMS_MAX_MESSAGE_LENGTH,
                "chunk of {} code points exceeds the cap",
                body.chars().count()
            );
            assert!(!body.trim().is_empty());
            assert_eq!(
                request.url,
                "https://mock.twilio.test/2010-04-01/Accounts/AC1234567890/Messages.json"
            );
        }
    }

    #[tokio::test]
    async fn rate_limited_honours_retry_after() {
        let mock = MockTransport::new(vec![Ok(HttpResponse {
            status: 429,
            body: serde_json::json!({"code": 20429, "message": "Too Many Requests"}).to_string(),
            retry_after: Some(7.0),
        })]);
        let result = adapter(&mock).send(FROM, "hello", None, None).await;

        assert!(!result.success);
        assert_eq!(result.error_kind.as_deref(), Some("rate_limited"));
        assert!(result.retryable);
        assert_eq!(result.retry_after, Some(7.0));
        assert!(
            result
                .error
                .as_deref()
                .unwrap_or_default()
                .contains("20429"),
            "{:?}",
            result.error
        );
    }

    #[tokio::test]
    async fn unauthorized_and_forbidden_are_not_retryable() {
        for status in [401u16, 403] {
            let mock = MockTransport::new(vec![response(
                status,
                serde_json::json!({"code": 20003, "message": "Authenticate"}),
            )]);
            let result = adapter(&mock).send(FROM, "hello", None, None).await;
            assert!(!result.success, "{status}");
            assert_eq!(result.error_kind.as_deref(), Some("forbidden"), "{status}");
            assert!(!result.retryable, "{status}");
        }
    }

    #[tokio::test]
    async fn not_found_and_server_errors_are_classified() {
        let missing = MockTransport::new(vec![response(
            404,
            serde_json::json!({"code": 20404, "message": "The requested resource was not found"}),
        )]);
        let result = adapter(&missing).send(FROM, "hello", None, None).await;
        assert_eq!(result.error_kind.as_deref(), Some("not_found"));
        assert!(!result.retryable);

        let server = MockTransport::new(vec![response(503, serde_json::Value::Null)]);
        let result = adapter(&server).send(FROM, "hello", None, None).await;
        assert_eq!(result.error_kind.as_deref(), Some("transient"));
        assert!(result.retryable);

        let down = MockTransport::new(vec![Err(TransportError::Request(
            "connection reset".to_string(),
        ))]);
        let result = adapter(&down).send(FROM, "hello", None, None).await;
        assert_eq!(result.error_kind.as_deref(), Some("transient"));
        assert!(result.retryable);
    }

    #[tokio::test]
    async fn twilio_400_is_bad_format_carrying_the_error_code() {
        let mock = MockTransport::new(vec![response(
            400,
            serde_json::json!({
                "code": 21610,
                "message": "Attempt to send to unsubscribed recipient",
                "more_info": "https://www.twilio.com/docs/errors/21610",
                "status": 400,
            }),
        )]);
        let result = adapter(&mock).send("+15550000003", "hi", None, None).await;

        assert!(!result.success);
        assert_eq!(result.error_kind.as_deref(), Some("bad_format"));
        assert!(!result.retryable);
        let error = result.error.as_deref().unwrap_or_default();
        assert!(error.contains("21610"), "{error}");
        assert!(error.contains("unsubscribed recipient"), "{error}");
    }

    #[tokio::test]
    async fn blank_content_is_rejected_without_a_request() {
        for content in ["", "   ", "\n\t "] {
            let mock = MockTransport::new(Vec::new());
            let result = adapter(&mock).send(FROM, content, None, None).await;
            assert!(!result.success, "{content:?}");
            assert_eq!(result.error_kind.as_deref(), Some("bad_format"));
            assert!(
                mock.requests().is_empty(),
                "blank content must not reach the wire: {content:?}"
            );
        }
    }

    #[tokio::test]
    async fn mid_way_failure_reports_the_sids_already_delivered() {
        let content = long_content();
        let expected = truncate_message(&content, SMS_MAX_MESSAGE_LENGTH, codepoint_len);
        assert!(expected.len() >= 2, "test content must split");

        let mock = MockTransport::new(vec![
            sent("SM1"),
            response(
                403,
                serde_json::json!({"code": 21610, "message": "unsubscribed recipient"}),
            ),
        ]);
        let result = adapter(&mock).send(FROM, &content, None, None).await;

        assert!(!result.success);
        assert_eq!(result.error_kind.as_deref(), Some("forbidden"));
        assert_eq!(result.continuation_message_ids, vec!["SM1"]);
        match result.partial_overflow() {
            Some((delivered, total)) => {
                assert_eq!(delivered, 1);
                assert_eq!(total as usize, expected.len());
            }
            None => panic!("failure after a delivered head must carry the partial marker"),
        }
    }

    #[tokio::test]
    async fn chat_info_is_the_phone_number_as_a_dm() {
        let mock = MockTransport::new(Vec::new());
        let info = adapter(&mock).get_chat_info("+15550000009").await;
        assert_eq!(info.name, "+15550000009");
        assert_eq!(info.chat_type, "dm");
        assert!(mock.requests().is_empty(), "Twilio has no chat metadata");
    }

    #[tokio::test]
    async fn capability_flags_match_hermes() {
        let mock = MockTransport::new(Vec::new());
        let adapter = adapter(&mock);
        assert!(adapter.splits_long_messages());
        assert_eq!(adapter.max_message_length(), SMS_MAX_MESSAGE_LENGTH);
        assert_eq!(adapter.max_message_length(), 1600);
        assert!(!adapter.supports_code_blocks());
        // Typing is a no-op that must never touch the wire.
        adapter.send_typing(FROM, None).await;
        assert!(mock.requests().is_empty());
    }

    const WEBHOOK_URL: &str = "https://gateway.test/webhooks/twilio";

    fn webhook_params() -> Vec<(&'static str, String)> {
        vec![
            ("To", FROM.to_string()),
            ("From", "+15550000002".to_string()),
            ("Body", "hello from a phone".to_string()),
            (
                "MessageSid",
                "SM0e3dd7f1a9c24f0d9a1c8c2f4b3a9e10".to_string(),
            ),
            ("AccountSid", ACCOUNT_SID.to_string()),
            ("NumSegments", "1".to_string()),
        ]
    }

    fn sign(token: &str, url: &str, params: &[(&str, String)]) -> String {
        let data = signature_payload(url, params);
        base64_encode(&hmac_sha1(token.as_bytes(), data.as_bytes()))
    }

    #[test]
    fn webhook_signature_verifies_and_rejects() {
        let mock = MockTransport::new(Vec::new());
        let adapter = adapter(&mock);
        let params = webhook_params();
        let good = sign(AUTH_TOKEN, WEBHOOK_URL, &params);
        assert!(adapter.verify_webhook(WEBHOOK_URL, &params, &good));

        // A different URL, a tampered body, a wrong token and an empty signature all fail.
        assert!(!adapter.verify_webhook("https://gateway.test/other", &params, &good));
        let tampered: Vec<(&str, String)> = params
            .iter()
            .map(|(key, value)| {
                if *key == "Body" {
                    (*key, "tampered".to_string())
                } else {
                    (*key, value.clone())
                }
            })
            .collect();
        assert!(!adapter.verify_webhook(WEBHOOK_URL, &tampered, &good));
        assert!(!adapter.verify_webhook(
            WEBHOOK_URL,
            &params,
            &sign("wrong-token", WEBHOOK_URL, &params)
        ));
        assert!(!adapter.verify_webhook(WEBHOOK_URL, &params, ""));
    }

    #[test]
    fn webhook_signature_orders_parameters_by_key() {
        let mock = MockTransport::new(Vec::new());
        let adapter = adapter(&mock);
        let params = webhook_params();
        let good = sign(AUTH_TOKEN, WEBHOOK_URL, &params);
        // Twilio's own ordering is the sorted one, so any wire order must verify.
        let mut shuffled = params.clone();
        shuffled.reverse();
        assert!(adapter.verify_webhook(WEBHOOK_URL, &shuffled, &good));
        assert_eq!(
            signature_payload(WEBHOOK_URL, &shuffled),
            signature_payload(WEBHOOK_URL, &params)
        );
    }

    #[test]
    fn empty_auth_token_fails_closed() {
        let mock = MockTransport::new(Vec::new());
        let adapter = SmsAdapter::new(mock, ACCOUNT_SID, "  ", FROM).with_api_base(API_BASE);
        let params = webhook_params();
        // Even a signature computed over the blank key must be refused.
        let blank_keyed = sign("  ", WEBHOOK_URL, &params);
        assert!(!adapter.verify_webhook(WEBHOOK_URL, &params, &blank_keyed));
        assert!(!adapter.verify_webhook(WEBHOOK_URL, &params, "anything"));
    }

    #[test]
    fn parse_webhook_builds_an_event_from_the_form() {
        let mock = MockTransport::new(Vec::new());
        let adapter = adapter(&mock);
        let params = webhook_params();
        let event = adapter
            .parse_webhook(&params)
            .expect("valid payload")
            .expect("an event");

        assert_eq!(event.text, "hello from a phone");
        assert_eq!(event.message_type, MessageType::Text);
        assert_eq!(event.source.platform.as_str(), "sms");
        assert_eq!(event.source.chat_id, "+15550000002");
        assert_eq!(event.source.chat_name.as_deref(), Some("+15550000002"));
        assert_eq!(event.source.chat_type, "dm");
        assert_eq!(event.source.user_id.as_deref(), Some("+15550000002"));
        assert_eq!(event.user_id.as_deref(), Some("+15550000002"));
        assert_eq!(event.user_name.as_deref(), Some("+15550000002"));
        assert_eq!(
            event.message_id.as_deref(),
            Some("SM0e3dd7f1a9c24f0d9a1c8c2f4b3a9e10")
        );
        assert_eq!(event.source.message_id, event.message_id);
        // A SID is not a numeric update id, so none is invented.
        assert_eq!(event.platform_update_id, None);
        match event.raw_message.as_ref() {
            Some(raw) => assert_eq!(raw["NumSegments"], serde_json::json!("1")),
            None => panic!("the raw form payload must be kept"),
        }
    }

    #[test]
    fn parse_webhook_skips_our_own_outbound_echo() {
        let mock = MockTransport::new(Vec::new());
        let adapter = adapter(&mock);
        let echo: Vec<(&str, String)> = vec![
            ("From", FROM.to_string()),
            ("To", "+15550000002".to_string()),
            ("Body", "a reply we just sent".to_string()),
            ("MessageSid", "SMecho".to_string()),
        ];
        assert_eq!(adapter.parse_webhook(&echo).expect("no error"), None);
    }

    #[test]
    fn parse_webhook_ignores_payloads_without_a_body() {
        let mock = MockTransport::new(Vec::new());
        let adapter = adapter(&mock);

        // A delivery-status callback: no Body at all.
        let status_callback: Vec<(&str, String)> = vec![
            ("MessageSid", "SMstatus".to_string()),
            ("MessageStatus", "delivered".to_string()),
            ("To", "+15550000002".to_string()),
        ];
        assert_eq!(
            adapter.parse_webhook(&status_callback).expect("no error"),
            None
        );
        assert_eq!(adapter.parse_webhook(&[]).expect("no error"), None);

        // A blank Body is not a message either.
        let blank: Vec<(&str, String)> = vec![
            ("From", "+15550000002".to_string()),
            ("Body", "   ".to_string()),
        ];
        assert_eq!(adapter.parse_webhook(&blank).expect("no error"), None);

        // A Body with no sender cannot be routed, so it is a rejection rather than a silent skip.
        let senderless: Vec<(&str, String)> = vec![("Body", "who sent this?".to_string())];
        assert!(adapter.parse_webhook(&senderless).is_err());
    }
}
