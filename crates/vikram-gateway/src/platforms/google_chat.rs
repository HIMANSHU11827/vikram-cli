//! Google Chat adapter — incoming webhook (text and Cards v2) plus webhook event parsing.
//!
//! Google offers two outbound modes: an **incoming webhook** (a URL that accepts a Message resource,
//! e.g. `{"text": …}`) and the **Chat API** with a service-account JWT. Only the webhook is
//! implemented: the API mode needs RSA signing, which this dependency-free crate does not have. That
//! is a documented gap, not a silent one.
//!
//! Rich cards go out through that same webhook. The webhook endpoint takes the very Message body the
//! Chat API's `spaces.messages.create` takes, so the reference's card payload — `{"cardsV2": [card]}`,
//! `send_card`, adapter.py:1122-1136 — is posted to it verbatim by
//! [`GoogleChatAdapter::send_card`]. What webhook mode cannot do is the *interactive* half: the
//! reference only ACKs card clicks (module docstring, adapter.py:7) and this adapter's inbound parser
//! handles `MESSAGE` events alone, so a card is display-only here.
//!
//! Inbound events arrive as HTTP POSTs carrying a Google-signed JWT. [`GoogleChatAdapter::verify_request`]
//! validates the `Authorization: Bearer <jwt>` header: RS256/ES256 signature against Google's
//! published OAuth2 certificates (`https://www.googleapis.com/oauth2/v3/certs`), `iss` exactly
//! `https://accounts.google.com`, and `aud` equal to the Cloud project number configured through
//! [`GoogleChatAdapter::with_project_number`]. It fails closed — no project number, no header, no
//! token, any verification error ⇒ `false`.
//!
//! The host owns the listener and **must** call `verify_request` before
//! [`GoogleChatAdapter::parse_webhook`]: parsing itself does no verification at all, so an
//! unverified body is arbitrary prompt text from anyone who can reach the endpoint.

use async_trait::async_trait;
use serde_json::{Value, json};
use std::sync::Arc;

use crate::event::{MessageEvent, MessageType, Platform, SessionSource};
use crate::helpers::{codepoint_len, truncate_message};
use crate::jwt::{JwtVerifier, bearer_token};
use crate::platform::{ChatInfo, PlatformAdapter, SendResult};
use crate::transport::SharedTransport;

pub const GOOGLE_CHAT_MAX_MESSAGE_LENGTH: usize = 4000;

/// Where Google publishes the certificates that sign its callback tokens.
pub const GOOGLE_CHAT_JWKS_URL: &str = "https://www.googleapis.com/oauth2/v3/certs";

/// The only `iss` a Google-signed callback token carries.
pub const GOOGLE_CHAT_ISSUER: &str = "https://accounts.google.com";

/// Google Chat adapter (incoming webhook out, HTTP-event webhooks in).
pub struct GoogleChatAdapter {
    transport: SharedTransport,
    webhook_url: String,
    verifier: JwtVerifier,
    /// The Cloud project number Google stamps into a callback token's `aud`; unset means
    /// [`GoogleChatAdapter::verify_request`] rejects everything.
    project_number: Option<String>,
}

impl GoogleChatAdapter {
    pub fn new(transport: SharedTransport, webhook_url: impl Into<String>) -> Self {
        let verifier = JwtVerifier::new(Arc::clone(&transport));
        Self {
            transport,
            webhook_url: webhook_url.into(),
            verifier,
            project_number: None,
        }
    }

    /// Set the Cloud project number a callback token's `aud` must equal (Google sends the numeric
    /// project id, e.g. `"123456789012"`). A blank value clears it, which turns
    /// [`GoogleChatAdapter::verify_request`] into a refusal rather than an open door.
    pub fn with_project_number(mut self, project_number: impl Into<String>) -> Self {
        let project_number = project_number.into();
        let trimmed = project_number.trim();
        self.project_number = if trimmed.is_empty() {
            None
        } else {
            Some(trimmed.to_string())
        };
        self
    }

    /// Whether `authorization` proves the request really came from Google Chat for this project.
    ///
    /// Strips the `Bearer ` scheme, then verifies the token against [`GOOGLE_CHAT_JWKS_URL`] with
    /// `iss` = [`GOOGLE_CHAT_ISSUER`] and `aud` = the configured project number. Every failure —
    /// no project number, no header, a malformed token, a bad signature, an expired token, the
    /// wrong issuer or audience, an unreachable JWKS — is `false`, because a webhook endpoint may
    /// only ever fail closed.
    ///
    /// Call this before [`GoogleChatAdapter::parse_webhook`]; parsing does no verification.
    ///
    /// Google additionally sends `email`/`azp` claims naming the calling identity, and hermes pins
    /// that email to a configured service account. This adapter does not: there is no configured
    /// service-account email to pin against, so the project-number audience is the whole check.
    pub async fn verify_request(&self, authorization: &str) -> bool {
        let Some(project_number) = self.project_number.as_deref() else {
            return false;
        };
        let Some(token) = bearer_token(authorization) else {
            return false;
        };
        self.verifier
            .verify(
                token,
                GOOGLE_CHAT_JWKS_URL,
                GOOGLE_CHAT_ISSUER,
                project_number,
            )
            .await
            .is_ok()
    }

    fn webhook_configured(&self) -> bool {
        let url = self.webhook_url.trim();
        url.starts_with("https://") && url.len() > "https://".len()
    }

    /// Parse one Google Chat event body. **This does not authenticate anything**: it only decodes.
    /// Call [`GoogleChatAdapter::verify_request`] on the request's `Authorization` header first —
    /// an unverified body is arbitrary text from whoever can reach the endpoint.
    pub fn parse_webhook(&self, body: &str) -> Result<Vec<MessageEvent>, String> {
        let root: Value =
            serde_json::from_str(body).map_err(|err| format!("invalid json: {err}"))?;
        if root.get("type").and_then(Value::as_str) != Some("MESSAGE") {
            return Ok(Vec::new());
        }
        let Some(message) = root.get("message") else {
            return Ok(Vec::new());
        };
        let sender = message.get("sender");
        if sender.and_then(|s| s.get("type")).and_then(Value::as_str) == Some("BOT") {
            return Ok(Vec::new());
        }
        let text = message
            .get("text")
            .and_then(Value::as_str)
            .unwrap_or_default()
            .to_string();
        let attachments = message
            .get("attachment")
            .and_then(Value::as_array)
            .cloned()
            .unwrap_or_default();
        if text.trim().is_empty() && attachments.is_empty() {
            return Ok(Vec::new());
        }

        let space = root.get("space");
        let chat_id = space
            .and_then(|s| s.get("name"))
            .and_then(Value::as_str)
            .or_else(|| {
                message
                    .get("space")
                    .and_then(|s| s.get("name"))
                    .and_then(Value::as_str)
            })
            .unwrap_or_default()
            .to_string();
        if chat_id.is_empty() {
            return Ok(Vec::new());
        }
        let chat_type = if space.and_then(|s| s.get("type")).and_then(Value::as_str) == Some("DM") {
            "dm"
        } else {
            "group"
        };

        let platform = Platform::new("google_chat").expect("valid platform");
        let mut source = SessionSource::new(platform, chat_id.clone());
        source.chat_type = chat_type.to_string();
        source.user_id = sender
            .and_then(|s| s.get("name"))
            .and_then(Value::as_str)
            .map(str::to_owned);
        source.user_name = sender
            .and_then(|s| s.get("displayName"))
            .and_then(Value::as_str)
            .map(str::to_owned);
        source.message_id = message
            .get("name")
            .and_then(Value::as_str)
            .map(str::to_owned);
        source.thread_id = message
            .get("thread")
            .and_then(|t| t.get("name"))
            .and_then(Value::as_str)
            .map(str::to_owned);

        let mut event = MessageEvent::new(text, source);
        event.user_id = event.source.user_id.clone();
        event.user_name = event.source.user_name.clone();
        event.message_id = event.source.message_id.clone();
        event.reply_to_message_id = message
            .get("quotedMessageMetadata")
            .and_then(|quoted| quoted.get("name"))
            .and_then(Value::as_str)
            .map(str::to_owned);
        event.platform_update_id = message
            .get("createTime")
            .and_then(Value::as_str)
            .and_then(|stamp| stamp.parse::<i64>().ok());
        event.raw_message = Some(root);
        for attachment in attachments {
            if let Some(content_type) = attachment.get("contentType").and_then(Value::as_str) {
                event.media_types.push(content_type.to_string());
            }
            if let Some(uri) = attachment.get("downloadUri").and_then(Value::as_str) {
                event.media_urls.push(uri.to_string());
            }
            if let Some(name) = attachment.get("contentName").and_then(Value::as_str) {
                event.media_filenames.push(name.to_string());
            }
        }
        if event.message_type == MessageType::Text && !event.media_types.is_empty() {
            event.message_type = MessageType::Document;
        }
        Ok(vec![event])
    }
}

#[async_trait]
impl PlatformAdapter for GoogleChatAdapter {
    async fn connect(&mut self, _is_reconnect: bool) -> bool {
        // A webhook has no identity endpoint; the URL either is usable or is not.
        self.webhook_configured()
    }

    async fn disconnect(&mut self) {}

    async fn send(
        &self,
        _chat_id: &str,
        content: &str,
        _reply_to: Option<&str>,
        metadata: Option<Value>,
    ) -> SendResult {
        if !self.webhook_configured() {
            return SendResult::failed(
                "google_chat webhook url is not configured",
                Some("bad_format"),
            );
        }
        if content.trim().is_empty() {
            return SendResult::failed("empty message content", Some("bad_format"));
        }
        let thread = metadata
            .as_ref()
            .and_then(|meta| meta.get("thread_id"))
            .and_then(Value::as_str)
            .map(str::to_owned);

        let chunks = truncate_message(content, GOOGLE_CHAT_MAX_MESSAGE_LENGTH, codepoint_len);
        let mut ids: Vec<String> = Vec::new();
        for chunk in chunks {
            let mut body = json!({ "text": chunk });
            if let Some(thread) = &thread {
                body["thread"] = json!({ "name": thread });
            }
            let request = crate::transport::HttpRequest::json(&self.webhook_url, &body);
            let response = match self.transport.execute(request).await {
                Ok(response) => response,
                Err(err) => {
                    return SendResult::failed(
                        format!("google_chat transport error: {err}"),
                        Some("transient"),
                    )
                    .retryable(None);
                }
            };
            if !response.ok() {
                return classify_failure(&response);
            }
            if let Ok(value) = response.json()
                && let Some(name) = value.get("name").and_then(Value::as_str)
            {
                ids.push(name.to_string());
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

    /// True: [`PlatformAdapter::send_card`] posts a real `cardsV2` message to the webhook.
    fn supports_cards(&self) -> bool {
        true
    }

    /// Send one Cards v2 message through the incoming webhook.
    ///
    /// The payload is the reference's, byte for byte: `send_card` builds
    /// `{"cardsV2": [card]}` and creates a message with it (adapter.py:1122-1129), and an incoming
    /// webhook takes exactly the Message resource `spaces.messages.create` takes — the same endpoint
    /// `send` already posts `{"text": …}` to. So the body is
    ///
    /// ```json
    /// {"cardsV2": [{"cardId": "hermes-card", "card": {"header": {…}, "sections": […]}}],
    ///  "text": "<fallback, only when the card carries one>"}
    /// ```
    ///
    /// with `cardsV2` always an array (Google's field is repeated; a single entry is wrapped, an
    /// array is passed through in order).
    ///
    /// Mapping decisions, all documented because the reference decides them elsewhere:
    /// - **shape**: one `card` argument may be a Cards v2 *entry* (`{"cardId": …, "card": {…}}`) or a
    ///   non-empty array of them. The reference always sends entries — `card_spec_to_cards_v2`
    ///   (`cards.py:182-190`) is what turns a card spec into `{"cardId": …, "card": {…}}` — so this
    ///   port validates entries and refuses anything else. It does not re-implement that
    ///   spec→Cards-v2 translation (snake_case → camelCase, widget kind mapping), because guessing
    ///   it would put an untranslated card on the wire and call it delivered: a bare
    ///   `{"sections": […]}` spec is refused with the wrapping it needs.
    /// - **fallback text**: a Message may carry `text` alongside `cardsV2` (what clients that cannot
    ///   render the card show, and what notification previews use). The reference's `send_card` sets
    ///   no `text`, but the trait's `send_card` gives no place to pass one either, so a non-blank
    ///   `text` on the entry is hoisted to the message-level `text` and removed from the entry (where
    ///   it is not part of the Cards v2 schema). Absent or blank ⇒ no `text` key, never `text: ""`.
    /// - **thread**: none. The reference threads a card when `metadata["thread_id"]` or its
    ///   `_last_inbound_thread` cache names one (`_resolve_thread_id`, adapter.py:1232-1240); the
    ///   trait's `send_card` carries no metadata and this adapter keeps no per-conversation state, so
    ///   a card is posted top-level rather than to a remembered thread it does not have.
    /// - **`chat_id`**: unused, exactly like [`PlatformAdapter::send`] here — an incoming webhook is
    ///   bound to one space when it is created, so a card cannot be routed anywhere else.
    ///
    /// Validation is local and makes no request: a non-object, an empty array, an entry that is not
    /// an object, and an entry with no non-empty `card` object are all `bad_format` refusals.
    ///
    /// Success is the webhook's 2xx, which is the acceptance the reference trusts too (`_create_message`
    /// returns `SendResult(success=True, message_id=resp.get("name"))`, adapter.py:1284-1292): the
    /// response's `name` becomes `message_id` when present, and `raw_response` echoes the `cardsV2`
    /// that was delivered (adapter.py:1129). A transport error is `transient`/retryable; a non-2xx
    /// goes through the adapter's shared [`classify_failure`] taxonomy. A webhook that rejects the
    /// card therefore reports the rejection — never a delivered card.
    async fn send_card(&self, _chat_id: &str, card: &Value) -> SendResult {
        if !self.webhook_configured() {
            return SendResult::failed(
                "google_chat webhook url is not configured",
                Some("bad_format"),
            );
        }
        let (entries, fallback_text) = match cards_v2_entries(card) {
            Ok(parsed) => parsed,
            Err(reason) => return SendResult::failed(reason, Some("bad_format")),
        };
        let mut body = json!({ "cardsV2": entries });
        if let Some(text) = fallback_text {
            body["text"] = Value::String(text);
        }
        let request = crate::transport::HttpRequest::json(&self.webhook_url, &body);
        let response = match self.transport.execute(request).await {
            Ok(response) => response,
            Err(err) => {
                return SendResult::failed(
                    format!("google_chat transport error: {err}"),
                    Some("transient"),
                )
                .retryable(None);
            }
        };
        if !response.ok() {
            return classify_failure(&response);
        }
        let parsed = response.json().unwrap_or(Value::Null);
        let message_id = parsed
            .get("name")
            .and_then(Value::as_str)
            .map(str::trim)
            .filter(|name| !name.is_empty())
            .map(str::to_string);
        card_success(message_id, &entries)
    }

    /// An incoming webhook cannot edit a message it already sent.
    async fn edit_message(
        &self,
        _chat_id: &str,
        _message_id: &str,
        _content: &str,
        _finalize: bool,
    ) -> SendResult {
        SendResult::failed(
            "google_chat incoming webhooks cannot edit messages; send a new one",
            Some("bad_format"),
        )
    }

    async fn get_chat_info(&self, chat_id: &str) -> ChatInfo {
        ChatInfo {
            name: chat_id.to_string(),
            chat_type: "channel".to_string(),
        }
    }

    async fn send_typing(&self, _chat_id: &str, _metadata: Option<Value>) {}
}

fn classify_failure(response: &crate::transport::HttpResponse) -> SendResult {
    let excerpt: String = response.body.chars().take(200).collect();
    let message = format!("google_chat returned {}: {excerpt}", response.status);
    match response.status {
        429 => SendResult::failed(message, Some("rate_limited")).retryable(response.retry_after),
        401 | 403 => SendResult::failed(message, Some("forbidden")),
        404 => SendResult::failed(message, Some("not_found")),
        400..=499 => SendResult::failed(message, Some("bad_format")),
        500..=599 => SendResult::failed(message, Some("transient")).retryable(None),
        _ => SendResult::failed(message, Some("unknown")),
    }
}

/// The success shape for a delivered card message.
///
/// A webhook 2xx is the acceptance: the response's `name` is the created message's resource name and
/// becomes `message_id` when present (the reference does the same with `resp.get("name")`,
/// adapter.py:1284-1292), and `raw_response` records the `cardsV2` that was delivered the way
/// `send_card` does (`result.raw_response = … {"cardsV2": body["cardsV2"]}`, adapter.py:1129).
fn card_success(message_id: Option<String>, entries: &[Value]) -> SendResult {
    SendResult {
        success: true,
        message_id,
        error: None,
        raw_response: Some(json!({ "cardsV2": entries })),
        retryable: false,
        retry_after: None,
        continuation_message_ids: Vec::new(),
        error_kind: None,
    }
}

/// Validate one caller-supplied card payload into the `cardsV2` array to send, plus the fallback
/// `text` a card may carry.
///
/// Accepted, and nothing else (`send_card`, adapter.py:1122-1123):
/// - one Cards v2 **entry** object — `{"cardId": …, "card": {…}}`, the exact shape
///   `card_spec_to_cards_v2` returns (`cards.py:182-190`);
/// - a non-empty array of such entries, kept in order.
///
/// Every other shape is an `Err` carrying the `bad_format` message to report, and no request is
/// made: a non-object, an empty array, an entry that is not an object, an empty entry, an entry whose
/// `card` is missing/empty/not an object. The last one is the useful refusal — a bare card spec
/// (`{"sections": […]}`) must be wrapped, because translating a spec into Cards v2 is the reference's
/// `card_spec_to_cards_v2` job and this port will not pretend it happened.
///
/// A non-blank `text` on an entry is a Message-level fallback, not part of the Cards v2 schema, so it
/// is lifted out to the message's `text` (first one wins) and removed from the entry.
fn cards_v2_entries(card: &Value) -> Result<(Vec<Value>, Option<String>), String> {
    let entries: Vec<Value> = match card {
        Value::Object(_) => vec![card.clone()],
        Value::Array(items) => {
            if items.is_empty() {
                return Err(
                    "google_chat refuses an empty cardsV2 array: send at least one card"
                        .to_string(),
                );
            }
            items.clone()
        }
        other => {
            return Err(format!(
                "google_chat sends a Cards v2 entry as a JSON object, or an array of them, not {}",
                json_type_name(other)
            ));
        }
    };

    let mut validated = Vec::with_capacity(entries.len());
    let mut fallback_text: Option<String> = None;
    for (index, entry) in entries.iter().enumerate() {
        let Some(object) = entry.as_object() else {
            return Err(format!(
                "google_chat cardsV2 entry #{index} is {}, not a JSON object",
                json_type_name(entry)
            ));
        };
        if object.is_empty() {
            return Err(format!(
                "google_chat cardsV2 entry #{index} is an empty object"
            ));
        }
        match object.get("card") {
            Some(Value::Object(body)) if !body.is_empty() => {}
            Some(Value::Object(_)) => {
                return Err(format!(
                    "google_chat cardsV2 entry #{index} has an empty \"card\" object"
                ));
            }
            Some(other) => {
                return Err(format!(
                    "google_chat cardsV2 entry #{index} has a \"card\" that is {}, not a JSON object",
                    json_type_name(other)
                ));
            }
            None => {
                return Err(format!(
                    "google_chat cardsV2 entry #{index} has no \"card\" object: wrap the card as \
                     {{\"cardId\": …, \"card\": {{\"sections\": […]}}}} (a bare card spec is not a \
                     Cards v2 entry)"
                ));
            }
        }
        if fallback_text.is_none()
            && let Some(text) = object
                .get("text")
                .and_then(Value::as_str)
                .map(str::trim)
                .filter(|text| !text.is_empty())
        {
            fallback_text = Some(text.to_string());
        }
        let mut entry = entry.clone();
        if let Some(map) = entry.as_object_mut() {
            map.remove("text");
        }
        validated.push(entry);
    }
    Ok((validated, fallback_text))
}

/// The JSON type of `value`, so a refusal says what arrived instead of echoing the payload.
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

#[cfg(test)]
mod tests {
    use super::*;
    use crate::jwt::test_support::{RSA_KID, rsa_jwks, sign_rs256};
    use crate::transport::{HttpMethod, HttpRequest, HttpResponse, Transport, TransportError};
    use async_trait::async_trait;
    use std::collections::VecDeque;
    use std::sync::{Arc, Mutex};

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

    fn build(responses: Vec<HttpResponse>) -> (GoogleChatAdapter, Arc<MockTransport>) {
        let mock = MockTransport::new(responses);
        let adapter = GoogleChatAdapter::new(
            Arc::clone(&mock) as SharedTransport,
            "https://chat.googleapis.com/v1/spaces/X/messages?key=k&token=t",
        );
        (adapter, mock)
    }

    fn ok(name: &str) -> HttpResponse {
        HttpResponse {
            status: 200,
            body: json!({ "name": name }).to_string(),
            retry_after: None,
        }
    }

    fn body_of(request: &HttpRequest) -> Value {
        serde_json::from_slice(request.body.as_ref().expect("body")).expect("json")
    }

    #[tokio::test]
    async fn send_posts_text_to_the_webhook() {
        let (adapter, mock) = build(vec![ok("spaces/X/messages/1")]);
        let result = adapter.send("spaces/X", "hello", None, None).await;
        assert!(result.success);
        assert_eq!(result.message_id.as_deref(), Some("spaces/X/messages/1"));
        let requests = mock.requests();
        assert_eq!(requests.len(), 1);
        assert_eq!(body_of(&requests[0])["text"], "hello");
    }

    #[tokio::test]
    async fn thread_metadata_is_forwarded() {
        let (adapter, mock) = build(vec![ok("spaces/X/messages/1")]);
        adapter
            .send(
                "spaces/X",
                "hi",
                None,
                Some(json!({"thread_id": "spaces/X/threads/T"})),
            )
            .await;
        let body = body_of(&mock.requests()[0]);
        assert_eq!(body["thread"]["name"], "spaces/X/threads/T");
    }

    #[tokio::test]
    async fn long_content_is_chunked() {
        let content = "x".repeat(GOOGLE_CHAT_MAX_MESSAGE_LENGTH + 100);
        let (adapter, mock) = build(vec![ok("a"), ok("b")]);
        let result = adapter.send("spaces/X", &content, None, None).await;
        assert!(result.success);
        assert_eq!(result.continuation_message_ids, vec!["a", "b"]);
        assert_eq!(mock.requests().len(), 2);
        for request in mock.requests() {
            let text = body_of(&request)["text"].as_str().unwrap().to_string();
            assert!(text.chars().count() <= GOOGLE_CHAT_MAX_MESSAGE_LENGTH);
        }
    }

    #[tokio::test]
    async fn rate_limits_are_retryable_and_auth_failures_are_not() {
        let (adapter, _) = build(vec![HttpResponse {
            status: 429,
            body: "slow down".into(),
            retry_after: Some(3.0),
        }]);
        let limited = adapter.send("spaces/X", "hi", None, None).await;
        assert!(limited.retryable);
        assert_eq!(limited.retry_after, Some(3.0));

        let (adapter, _) = build(vec![HttpResponse {
            status: 403,
            body: "no".into(),
            retry_after: None,
        }]);
        let refused = adapter.send("spaces/X", "hi", None, None).await;
        assert!(!refused.retryable);
        assert_eq!(refused.error_kind.as_deref(), Some("forbidden"));
    }

    #[tokio::test]
    async fn blank_content_makes_no_request() {
        let (adapter, mock) = build(vec![]);
        let result = adapter.send("spaces/X", "   ", None, None).await;
        assert!(!result.success);
        assert_eq!(result.error_kind.as_deref(), Some("bad_format"));
        assert!(mock.requests().is_empty());
    }

    #[tokio::test]
    async fn an_empty_success_body_still_counts_as_success() {
        let (adapter, _) = build(vec![HttpResponse {
            status: 200,
            body: String::new(),
            retry_after: None,
        }]);
        let result = adapter.send("spaces/X", "hi", None, None).await;
        assert!(result.success);
        assert!(result.message_id.is_none());
    }

    #[test]
    fn webhook_events_become_events_and_bots_are_skipped() {
        let (adapter, _) = build(vec![]);
        let body = json!({
            "type": "MESSAGE",
            "message": {
                "name": "spaces/X/messages/M1",
                "text": "hello",
                "createTime": "1700000000",
                "sender": {"name": "users/1", "displayName": "Asha", "type": "HUMAN"},
                "thread": {"name": "spaces/X/threads/T"},
                "quotedMessageMetadata": {"name": "spaces/X/messages/M0"}
            },
            "space": {"name": "spaces/X", "type": "DM"}
        })
        .to_string();
        let events = adapter.parse_webhook(&body).unwrap();
        assert_eq!(events.len(), 1);
        let event = &events[0];
        assert_eq!(event.text, "hello");
        assert_eq!(event.source.chat_type, "dm");
        assert_eq!(event.source.chat_id, "spaces/X");
        assert_eq!(event.user_id.as_deref(), Some("users/1"));
        assert_eq!(
            event.reply_to_message_id.as_deref(),
            Some("spaces/X/messages/M0")
        );

        let bot = json!({
            "type": "MESSAGE",
            "message": {"text": "beep", "sender": {"type": "BOT"}, "name": "m"},
            "space": {"name": "spaces/X"}
        })
        .to_string();
        assert!(adapter.parse_webhook(&bot).unwrap().is_empty());
        assert!(
            adapter
                .parse_webhook(r#"{"type":"ADDED_TO_SPACE"}"#)
                .unwrap()
                .is_empty()
        );
        assert!(adapter.parse_webhook("not json").is_err());
    }

    #[test]
    fn attachments_surface_as_media() {
        let (adapter, _) = build(vec![]);
        let body = json!({
            "type": "MESSAGE",
            "message": {
                "name": "m",
                "text": "see file",
                "sender": {"name": "u"},
                "attachment": [{"contentType": "image/png", "downloadUri": "https://x/y.png", "contentName": "y.png"}]
            },
            "space": {"name": "spaces/X"}
        })
        .to_string();
        let events = adapter.parse_webhook(&body).unwrap();
        assert_eq!(events[0].media_types, vec!["image/png"]);
        assert_eq!(events[0].media_urls, vec!["https://x/y.png"]);
        assert_eq!(events[0].media_filenames, vec!["y.png"]);
    }

    #[tokio::test]
    async fn edit_is_an_honest_refusal_and_typing_is_a_noop() {
        let (adapter, mock) = build(vec![]);
        let edit = adapter.edit_message("spaces/X", "m", "new", false).await;
        assert!(!edit.success);
        adapter.send_typing("spaces/X", None).await;
        assert!(mock.requests().is_empty());
    }

    // -- Cards v2 outbound -------------------------------------------------------------------

    /// A Cards v2 entry exactly as the reference's `card_spec_to_cards_v2` builds one
    /// (cards.py:182-190): the card body under `card`, wrapped with a card id.
    fn cards_v2_entry() -> Value {
        json!({
            "cardId": "hermes-card",
            "card": {
                "header": { "title": "Deploy", "subtitle": "prod" },
                "sections": [{
                    "widgets": [
                        { "textParagraph": { "text": "Deploy to prod?" } },
                        { "buttonList": { "buttons": [{ "text": "Approve", "onClick": { "action": { "function": "approve" } } }] } },
                    ],
                }],
            },
        })
    }

    /// The webhook URL [`build`] configures.
    const WEBHOOK_URL: &str = "https://chat.googleapis.com/v1/spaces/X/messages?key=k&token=t";

    #[test]
    fn supports_cards_reports_cards_v2_support() {
        let (adapter, _) = build(vec![]);
        assert!(adapter.supports_cards());
    }

    #[tokio::test]
    async fn send_card_posts_a_cards_v2_message_to_the_webhook() {
        let (adapter, mock) = build(vec![ok("spaces/X/messages/C1")]);
        let card = cards_v2_entry();
        let result = adapter.send_card("spaces/X", &card).await;

        assert!(result.success, "{:?}", result.error);
        assert_eq!(result.message_id.as_deref(), Some("spaces/X/messages/C1"));
        assert!(result.error.is_none());
        assert!(result.error_kind.is_none());
        assert!(result.continuation_message_ids.is_empty());
        // The delivered cardsV2 is echoed back the way the reference records it (adapter.py:1129).
        assert_eq!(
            result.raw_response,
            Some(json!({ "cardsV2": [card.clone()] }))
        );

        let requests = mock.requests();
        assert_eq!(requests.len(), 1);
        assert_eq!(requests[0].method, HttpMethod::Post);
        assert_eq!(requests[0].url, WEBHOOK_URL);
        let body = body_of(&requests[0]);
        // A single entry is wrapped in the repeated field.
        assert_eq!(body["cardsV2"], json!([card]));
        // No fallback text was carried, and no thread is invented.
        assert!(body.get("text").is_none());
        assert!(body.get("thread").is_none());
    }

    #[tokio::test]
    async fn send_card_sends_an_array_of_entries_in_order() {
        let (adapter, mock) = build(vec![ok("spaces/X/messages/C2")]);
        let first = cards_v2_entry();
        let second = json!({ "cardId": "second", "card": { "sections": [{ "widgets": [] }] } });
        let result = adapter
            .send_card("spaces/X", &json!([first.clone(), second.clone()]))
            .await;

        assert!(result.success, "{:?}", result.error);
        let body = body_of(&mock.requests()[0]);
        assert_eq!(body["cardsV2"], json!([first, second]));
    }

    #[tokio::test]
    async fn a_card_fallback_text_becomes_the_message_text() {
        let (adapter, mock) = build(vec![ok("n")]);
        let mut entry = cards_v2_entry();
        entry["text"] = json!("  Deploy to prod?  ");
        let result = adapter.send_card("spaces/X", &entry).await;
        assert!(result.success, "{:?}", result.error);

        let body = body_of(&mock.requests()[0]);
        // Trimmed onto the Message's own `text`, which is the fallback notification/clients show.
        assert_eq!(body["text"], json!("Deploy to prod?"));
        // `text` is a Message field, not a Cards v2 one, so the entry is sent without it.
        assert!(body["cardsV2"][0].get("text").is_none());
        assert_eq!(body["cardsV2"][0]["card"], entry["card"]);

        // A blank one is omitted rather than sent as an empty string.
        let (adapter, mock) = build(vec![ok("n")]);
        let mut blank = cards_v2_entry();
        blank["text"] = json!("   ");
        assert!(adapter.send_card("spaces/X", &blank).await.success);
        assert!(body_of(&mock.requests()[0]).get("text").is_none());
    }

    #[tokio::test]
    async fn send_card_refuses_a_payload_that_is_not_a_cards_v2_entry() {
        // (card, the phrase the refusal must carry)
        let cases = [
            (json!("not a card"), "not a string"),
            (json!(7), "not a number"),
            (Value::Null, "not null"),
            (json!([]), "empty cardsV2 array"),
            (json!({}), "is an empty object"),
            (json!([1, 2]), "entry #0 is a number, not a JSON object"),
            (json!({"card": {}}), "has an empty \"card\" object"),
            (json!({"card": "nope"}), "has a \"card\" that is a string"),
            (json!({"cardId": "c1"}), "has no \"card\" object"),
            // A bare card spec is not a Cards v2 entry: translating one is `card_spec_to_cards_v2`'s
            // job (cards.py:182-190) and this port refuses rather than half-translating.
            (
                json!({"sections": [{"widgets": []}]}),
                "has no \"card\" object",
            ),
        ];
        for (card, needle) in cases {
            let (adapter, mock) = build(vec![]);
            let result = adapter.send_card("spaces/X", &card).await;
            assert!(!result.success, "{card} must be refused");
            assert_eq!(result.error_kind.as_deref(), Some("bad_format"), "{card}");
            assert!(!result.retryable, "{card}");
            assert!(result.message_id.is_none(), "{card}");
            let error = result.error.unwrap_or_default();
            assert!(error.contains(needle), "{card}: {error}");
            assert!(
                mock.requests().is_empty(),
                "{card} must be refused before any request"
            );
        }
    }

    #[tokio::test]
    async fn send_card_refuses_without_a_webhook_url() {
        let mock = MockTransport::new(vec![]);
        // An incoming webhook URL is https://…; anything else cannot address a space.
        let adapter = GoogleChatAdapter::new(
            Arc::clone(&mock) as SharedTransport,
            "http://insecure.test/v1/spaces/X/messages",
        );
        for card in [cards_v2_entry(), json!("not a card")] {
            let result = adapter.send_card("spaces/X", &card).await;
            assert!(!result.success, "{card}");
            assert_eq!(result.error_kind.as_deref(), Some("bad_format"));
            assert_eq!(
                result.error.as_deref(),
                Some("google_chat webhook url is not configured")
            );
        }
        assert!(mock.requests().is_empty(), "no request may be made");
    }

    #[tokio::test]
    async fn card_failures_use_the_shared_taxonomy() {
        let card = cards_v2_entry();

        let (adapter, _) = build(vec![HttpResponse {
            status: 429,
            body: "slow down".into(),
            retry_after: Some(4.0),
        }]);
        let limited = adapter.send_card("spaces/X", &card).await;
        assert!(!limited.success);
        assert_eq!(limited.error_kind.as_deref(), Some("rate_limited"));
        assert!(limited.retryable);
        assert_eq!(limited.retry_after, Some(4.0));
        assert!(limited.message_id.is_none());
        assert!(limited.raw_response.is_none());

        let (adapter, _) = build(vec![HttpResponse {
            status: 403,
            body: "no".into(),
            retry_after: None,
        }]);
        let refused = adapter.send_card("spaces/X", &card).await;
        assert!(!refused.success);
        assert_eq!(refused.error_kind.as_deref(), Some("forbidden"));
        assert!(!refused.retryable);
        assert!(
            refused.error.as_deref().unwrap_or_default().contains("403"),
            "{:?}",
            refused.error
        );

        // A webhook that rejects the payload reports the rejection, not a delivered card.
        let (adapter, _) = build(vec![HttpResponse {
            status: 400,
            body: "Invalid JSON payload received. Unknown name \"card\"".into(),
            retry_after: None,
        }]);
        let rejected = adapter.send_card("spaces/X", &card).await;
        assert!(!rejected.success);
        assert_eq!(rejected.error_kind.as_deref(), Some("bad_format"));
        assert!(
            rejected
                .error
                .as_deref()
                .unwrap_or_default()
                .contains("Invalid JSON payload"),
            "{:?}",
            rejected.error
        );

        let (adapter, _) = build(vec![HttpResponse {
            status: 503,
            body: "unavailable".into(),
            retry_after: None,
        }]);
        let down = adapter.send_card("spaces/X", &card).await;
        assert_eq!(down.error_kind.as_deref(), Some("transient"));
        assert!(down.retryable);

        // A transport that never answered at all is transient too.
        let (adapter, _) = build(vec![]);
        let lost = adapter.send_card("spaces/X", &card).await;
        assert!(!lost.success);
        assert_eq!(lost.error_kind.as_deref(), Some("transient"));
        assert!(lost.retryable);
        assert!(
            lost.error
                .as_deref()
                .unwrap_or_default()
                .contains("transport error"),
            "{:?}",
            lost.error
        );
    }

    #[tokio::test]
    async fn a_card_success_without_a_message_name_still_records_the_cards_v2() {
        let (adapter, _) = build(vec![HttpResponse {
            status: 200,
            body: String::new(),
            retry_after: None,
        }]);
        let card = cards_v2_entry();
        let result = adapter.send_card("spaces/X", &card).await;
        assert!(result.success);
        assert!(result.message_id.is_none());
        assert_eq!(result.raw_response, Some(json!({ "cardsV2": [card] })));
    }

    #[test]
    fn card_helpers_validate_entries_and_hoist_the_fallback_text() {
        let entry = cards_v2_entry();
        let (entries, text) = cards_v2_entries(&entry).unwrap_or_default();
        assert_eq!(entries, vec![entry.clone()]);
        assert_eq!(text, None);

        let (entries, text) =
            cards_v2_entries(&json!([entry.clone(), entry.clone()])).unwrap_or_default();
        assert_eq!(entries.len(), 2);
        assert_eq!(text, None);

        let mut with_text = entry.clone();
        with_text["text"] = json!(" fallback ");
        let (entries, text) = cards_v2_entries(&with_text).unwrap_or_default();
        assert_eq!(text.as_deref(), Some("fallback"));
        assert!(entries[0].get("text").is_none());
        assert_eq!(entries[0]["card"], entry["card"]);

        assert_eq!(json_type_name(&json!("x")), "a string");
        assert_eq!(json_type_name(&json!(1.5)), "a number");
        assert_eq!(json_type_name(&json!(false)), "a boolean");
        assert_eq!(json_type_name(&json!({})), "an object");
    }

    // -- callback JWT verification -----------------------------------------------------------

    const PROJECT_NUMBER: &str = "123456789012";

    /// The JWKS response every verification test is served.
    fn jwks_response() -> HttpResponse {
        HttpResponse {
            status: 200,
            body: rsa_jwks().to_string(),
            retry_after: None,
        }
    }

    /// Claims shaped like the ones Google puts in a Chat callback token.
    fn google_claims(project_number: &str) -> Value {
        json!({
            "iss": "https://accounts.google.com",
            "azp": "1234567890-abc.apps.googleusercontent.com",
            "aud": project_number,
            "sub": "1234567890",
            "email": "chat@system.gserviceaccount.com",
            "email_verified": true,
            // Well past any test run's clock: 2096.
            "exp": 4_000_000_000u64,
        })
    }

    /// An adapter over a transport that serves the fixture JWKS, with `project_number` configured.
    fn verifying_adapter(project_number: &str) -> (GoogleChatAdapter, Arc<MockTransport>) {
        let mock = MockTransport::new(vec![jwks_response()]);
        let adapter = GoogleChatAdapter::new(
            Arc::clone(&mock) as SharedTransport,
            "https://chat.googleapis.com/v1/spaces/X/messages?key=k&token=t",
        )
        .with_project_number(project_number);
        (adapter, mock)
    }

    #[tokio::test]
    async fn a_google_signed_authorization_header_is_accepted() {
        let (adapter, mock) = verifying_adapter(PROJECT_NUMBER);
        let token = sign_rs256(RSA_KID, &google_claims(PROJECT_NUMBER));

        assert!(adapter.verify_request(&format!("Bearer {token}")).await);
        let requests = mock.requests();
        assert_eq!(requests.len(), 1);
        assert_eq!(requests[0].method, HttpMethod::Get);
        assert_eq!(requests[0].url, GOOGLE_CHAT_JWKS_URL);

        // The scheme is case-insensitive; the token is what matters.
        assert!(adapter.verify_request(&format!("bearer {token}")).await);
        // The JWKS is cached, so the second check made no further request.
        assert_eq!(mock.requests().len(), 1);
    }

    /// Re-encode `token`'s payload, keeping its header and signature, so the signature no longer
    /// covers the claims that get read.
    fn rewrap_payload(token: &str, payload: &Value) -> String {
        let mut parts = token.split('.');
        let header = parts.next().unwrap_or_default();
        let signature = parts.nth(1).unwrap_or_default();
        let body = crate::jwt::base64url_encode(payload.to_string().as_bytes());
        format!("{header}.{body}.{signature}")
    }

    #[tokio::test]
    async fn a_missing_or_garbage_authorization_header_is_rejected() {
        let (adapter, mock) = verifying_adapter(PROJECT_NUMBER);

        for header in [
            "",
            " ",
            "Bearer",
            "Bearer ",
            "Bearer   ",
            "garbage",
            "Basic dXNlcjpwYXNz",
            "Bearer not.a.token",
            "Bearer ZXlKaGJHY2lPaUpTVXpJMU5pSjk.e30.c2ln",
        ] {
            assert!(
                !adapter.verify_request(header).await,
                "{header:?} must not verify"
            );
        }
        // None of those had a decodable header, so the JWKS was never even fetched.
        assert!(mock.requests().is_empty());

        // A token for another project, one from another issuer, one that expired, one whose payload
        // was swapped under a valid signature, and one signed with a key Google never published.
        let mut other_issuer = google_claims(PROJECT_NUMBER);
        other_issuer["iss"] = json!("https://accounts.google.com.evil.test");
        let mut expired = google_claims(PROJECT_NUMBER);
        expired["exp"] = json!(1_000_000_000u64);
        let signed = sign_rs256(RSA_KID, &google_claims(PROJECT_NUMBER));

        let victims = [
            sign_rs256(RSA_KID, &google_claims("999999999999")),
            sign_rs256(RSA_KID, &other_issuer),
            sign_rs256(RSA_KID, &expired),
            rewrap_payload(
                &signed,
                &json!({"iss": GOOGLE_CHAT_ISSUER, "sub": "someone-else"}),
            ),
            sign_rs256(
                "a-kid-google-never-published",
                &google_claims(PROJECT_NUMBER),
            ),
        ];
        for token in victims {
            assert!(
                !adapter.verify_request(&format!("Bearer {token}")).await,
                "token must not verify: {token}"
            );
        }
        // All of those reused one cached JWKS.
        assert_eq!(mock.requests().len(), 1);
    }

    #[tokio::test]
    async fn verification_without_a_project_number_fails_closed() {
        // Never configured.
        let mock = MockTransport::new(vec![jwks_response()]);
        let adapter = GoogleChatAdapter::new(
            Arc::clone(&mock) as SharedTransport,
            "https://chat.googleapis.com/v1/spaces/X/messages?key=k&token=t",
        );
        let token = sign_rs256(RSA_KID, &google_claims(PROJECT_NUMBER));
        assert!(!adapter.verify_request(&format!("Bearer {token}")).await);

        // Configured as blank, which clears it again.
        let adapter = adapter.with_project_number("   ");
        assert!(!adapter.verify_request(&format!("Bearer {token}")).await);
        assert!(
            mock.requests().is_empty(),
            "an unconfigured adapter must not fetch"
        );

        // A project number that matches the token still verifies once it is configured.
        let adapter = adapter.with_project_number(PROJECT_NUMBER);
        assert!(adapter.verify_request(&format!("Bearer {token}")).await);
    }

    #[tokio::test]
    async fn a_jwks_outage_is_a_refusal_not_a_pass() {
        let mock = MockTransport::new(vec![HttpResponse {
            status: 503,
            body: "unavailable".into(),
            retry_after: Some(5.0),
        }]);
        let adapter = GoogleChatAdapter::new(
            Arc::clone(&mock) as SharedTransport,
            "https://chat.googleapis.com/v1/spaces/X/messages?key=k&token=t",
        )
        .with_project_number(PROJECT_NUMBER);
        let token = sign_rs256(RSA_KID, &google_claims(PROJECT_NUMBER));
        assert!(!adapter.verify_request(&format!("Bearer {token}")).await);
        assert_eq!(mock.requests().len(), 1);
    }
}
