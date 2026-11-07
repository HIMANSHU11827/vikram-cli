//! Microsoft Teams adapter — Bot Framework via client-credentials OAuth.
//!
//! Rust port of the wire half of hermes-agent `plugins/platforms/teams/adapter.py`: the
//! client-credentials token from `login.microsoftonline.com`, the connector's
//! `POST {serviceUrl}/v3/conversations/{id}/activities` message activity, the `typing` activity,
//! activity update (`PUT …/activities/{id}`) and activity delete (`DELETE …/activities/{id}`), plus
//! the `message` activity Bot Framework delivers to the bot's `/api/messages` endpoint.
//!
//! Everything hermes builds on the `microsoft-teams-apps` SDK is re-implemented here directly
//! against the Bot Framework REST surface over [`crate::transport::Transport`], so the adapter
//! stays testable with no network, no Azure app registration and no SDK.
//!
//! Ported behaviour, with the hermes origin:
//! - Bot Framework bearer token: `POST {login_base}/{tenant}/oauth2/v2.0/token` with the
//!   client-credentials grant and the `https://api.botframework.com/.default` scope, cached for
//!   `expires_in - 60` seconds and reused across sends (`_get_botframework_token`,
//!   `_bf_token_request`).
//! - Outbound text: `{"type": "message", "text": …}` posted as a conversation activity, chunked at
//!   [`TEAMS_MAX_MESSAGE_LENGTH`] code points through [`truncate_message`] and delivered
//!   sequentially (`send`, `MAX_MESSAGE_LENGTH`, `splits_long_messages`).
//! - `send_typing` as a best-effort `{"type": "typing"}` activity, failures swallowed.
//! - Outbound media: one `message` activity carrying `attachments: [{contentType, contentUrl}]`, the
//!   caption as the activity `text` and `replyToId` when a thread anchor was given. URL sources are
//!   attached by reference, held bytes become a `data:<mime>;base64,…` content URL
//!   (`_send_media_attachment`, adapter.py:745-773; the per-kind defaults come from
//!   `send_image`/`send_video`/`send_voice`/`send_document` at adapter.py:775-794).
//! - Outbound Adaptive Cards: one `message` activity carrying
//!   `attachments: [{contentType: "application/vnd.microsoft.card.adaptive", content: <card>}]`
//!   ([`PlatformAdapter::send_card`]), which is what the reference's `_send_card` puts on the wire
//!   — `MessageActivityInput().add_card(card)` over an SDK `AdaptiveCard` (`_send_card`,
//!   adapter.py:604-609; the typed, versioned card at adapter.py:633 and adapter.py:709).
//! - Inbound `message` activities: text, author, conversation type, `replyToId` and the activity
//!   `serviceUrl` (`_on_message`).
//! - Bot Framework JWT verification ([`TeamsAdapter::verify_request`]): the `Authorization: Bearer
//!   <jwt>` header is checked against the keys the Bot Framework publishes
//!   ([`BOT_FRAMEWORK_JWKS_URL`]), with `iss` exactly [`BOT_FRAMEWORK_ISSUER`] and `aud` the bot's
//!   own `client_id`. It fails closed. hermes delegates this to the `microsoft-teams-apps` SDK;
//!   here it is [`crate::jwt::JwtVerifier`] over the same [`crate::transport::Transport`], so it is
//!   testable with no Azure app registration.
//!
//! Deliberately not ported (all need the SDK, a listening webhook server, or process state this
//! stateless wire adapter does not own):
//! - the aiohttp listener on `POST /api/messages` (port 3978 by default), its 1 MiB body cap, the
//!   `/health` route and `web.request.json()` coercion — the host owns the listener;
//! - the SDK `App`/`ActivityContext` wiring, `initialize()`, plugin `on_*` handlers, the
//!   exec-approval card flow and its click allowlist (`_send_exec_approval_prompt`,
//!   `_on_card_action`, `_card_action_denied`). Outbound Adaptive Cards *are* ported — only the
//!   invoke/click half of the card surface is not, so a card sent here is display-only;
//! - inbound media: attachment download/caching (connector bearer auth, SharePoint
//!   `download.info` pre-authed URLs) and the `<attachment>` markup rewrite — outbound media is
//!   ported, see [`PlatformAdapter::send_media`];
//! - conversation-reference caching for proactive cards, the `require_mention` gate, the `<at>`
//!   mention-stripping rewrite, the `git`-style dedup cache (`MessageDeduplicator`), the
//!   `TEAMS_ALLOWED_USERS` admission policy and per-chat serial processing — the runner owns
//!   admission and dedup;
//! - the region allowlist (`smba.trafficmanager.net`, `smba.infra.gov.teams.microsoft.us`) and the
//!   conversation-id charset check: see the trust note on [`TeamsAdapter::send`].
//!
//! Two trust notes the caller has to honour, because this adapter cannot:
//!
//! 1. the `serviceUrl` a send targets arrives from untrusted input (a webhook activity or a
//!    `metadata` value). It is the host's job to only ever forward a `service_url` that came from an
//!    already-JWT-validated activity, otherwise the Bot Framework bearer token could be sent to an
//!    attacker-controlled host;
//! 2. the adapter never invents inbound authenticity: [`TeamsAdapter::verify_request`] is the only
//!    proof an activity is really from the Bot Framework, and the host must call it before
//!    [`TeamsAdapter::parse_webhook`], which parses without verifying.

use async_trait::async_trait;
use serde_json::{Value, json};
use std::sync::{Arc, Mutex, MutexGuard};
use std::time::{Duration, Instant};

use crate::event::{MessageEvent, Platform, SessionSource};
use crate::helpers::{codepoint_len, truncate_message};
use crate::jwt::{JwtVerifier, bearer_token};
use crate::platform::{
    ChatInfo, MediaKind, MediaSource, OutboundMedia, PlatformAdapter, SendResult,
};
use crate::transport::{HttpRequest, SharedTransport};

/// Teams' per-message text cap in code points (hermes `TeamsAdapter.MAX_MESSAGE_LENGTH`, "~28 KB");
/// longer answers are chunked by [`TeamsAdapter::send`].
pub const TEAMS_MAX_MESSAGE_LENGTH: usize = 28000;

/// Largest attachment this adapter will put in a `data:` content URL: 28 MB, the documented size
/// limit for a file attached to a Teams message (`MessageSizeTooBig` is what the connector answers
/// past it).
///
/// The hermes reference does not cap the file it reads (`_send_media_attachment`,
/// adapter.py:762-765): it base64-encodes whatever `Path(path).read_bytes()` returned and lets the
/// connector reject an oversized activity. Refusing locally keeps a 100 MB file from growing a
/// 133 MB JSON body on the way to a certain 413, and matches the limit the connector enforces. The
/// cap counts the raw bytes, not their base64 form — base64 inflates by 4/3, so a file close to the
/// cap can still be refused by the connector as `too_long`.
pub const TEAMS_MAX_ATTACHMENT_BYTES: usize = 28 * 1024 * 1024;

/// The public Azure AD (Entra ID) token endpoint base; sovereign clouds override it through
/// [`TeamsAdapter::with_login_base`] (hermes builds the URL inline in `_bf_token_request`).
pub const TEAMS_LOGIN_BASE: &str = "https://login.microsoftonline.com";

/// The global Bot Framework connector base a bot's activities are posted to; regional/government
/// deployments override it through [`TeamsAdapter::with_service_url`] (`_DEFAULT_TEAMS_SERVICE_URL`).
pub const DEFAULT_SERVICE_URL: &str = "https://smba.trafficmanager.net/teams";

/// The only scope that mints a Bot Framework connector token (hermes `_BF_TOKEN_SCOPE`).
pub const BOT_FRAMEWORK_SCOPE: &str = "https://api.botframework.com/.default";

/// Where the Bot Framework publishes the keys that sign the JWTs on inbound activities (the
/// `jwks_uri` of its OpenID configuration).
pub const BOT_FRAMEWORK_JWKS_URL: &str = "https://login.botframework.com/v1/.well-known/keys";

/// The only `iss` a Bot Framework activity token carries.
pub const BOT_FRAMEWORK_ISSUER: &str = "https://api.botframework.com";

/// The attachment content type a Teams Adaptive Card is delivered under.
///
/// hermes never spells it: it hands an SDK [`AdaptiveCard`](https://pypi.org/project/microsoft-teams-api/)
/// model to `MessageActivityInput().add_card(card)` (`_send_card`, adapter.py:604-609) and the SDK
/// serializes that model as the Bot Framework adaptive-card attachment, whose `contentType` is this
/// value and whose `content` is the card object itself.
pub const TEAMS_ADAPTIVE_CARD_CONTENT_TYPE: &str = "application/vnd.microsoft.card.adaptive";

/// The `type` an Adaptive Card declares.
///
/// The SDK model the reference sends is always typed (`AdaptiveCard().with_version("1.4")…`,
/// adapter.py:633 and adapter.py:709), so the reference can never put a type-less card on the wire.
/// A caller handing this port raw JSON gets [`TEAMS_ADAPTIVE_CARD_TYPE`] enforced instead — see
/// [`adaptive_card_content`].
pub const TEAMS_ADAPTIVE_CARD_TYPE: &str = "AdaptiveCard";

/// The gateway platform name. `event::platform_names` has no `TEAMS` entry yet and this file may not
/// edit it, so the name is stated here once.
const TEAMS_PLATFORM_NAME: &str = "teams";

/// A token is renewed this long before Azure AD says it expires, so an in-flight activity post never
/// races the deadline (hermes keeps a five-minute margin for attachments; one minute matches the
/// Feishu adapter's margin and Azure's own guidance for a short-lived connector token).
const TOKEN_SAFETY_MARGIN_SECS: i64 = 60;

/// Azure AD's default when a token response omits `expires_in`.
const DEFAULT_TOKEN_TTL_SECS: i64 = 3600;

/// One Bot Framework access token plus the instant it stops being usable.
#[derive(Debug, Clone)]
pub struct CachedToken {
    /// The `access_token` Azure AD issued.
    pub value: String,
    /// When the token must be replaced: issue time + `expires_in` − [`TOKEN_SAFETY_MARGIN_SECS`].
    pub expires_at: Instant,
}

/// Microsoft Teams adapter (Bot Framework connector).
///
/// Holds one shared [`crate::transport::Transport`], the Azure AD app identity, the token cache and
/// the connector base URL. The cache is a single slot behind a [`Mutex`] so sends from several tasks
/// reuse one token; the mutex is never held across an `await`.
///
/// It also holds the [`JwtVerifier`] that checks inbound activity tokens — see
/// [`TeamsAdapter::verify_request`].
pub struct TeamsAdapter {
    transport: SharedTransport,
    tenant_id: String,
    client_id: String,
    client_secret: String,
    login_base: String,
    token: Mutex<Option<CachedToken>>,
    service_url: String,
    verifier: JwtVerifier,
}

impl TeamsAdapter {
    /// Build an adapter for the app registration `client_id`/`client_secret` inside `tenant_id`,
    /// over `transport`, talking to [`TEAMS_LOGIN_BASE`] and [`DEFAULT_SERVICE_URL`].
    ///
    /// An empty credential is not rejected here: [`TeamsAdapter::connect`] reports it as a failed
    /// token request, which is where Azure AD would refuse it anyway. An empty `client_id` does,
    /// however, make [`TeamsAdapter::verify_request`] reject every inbound activity — there is no
    /// audience to check the token against.
    pub fn new(
        transport: SharedTransport,
        tenant_id: impl Into<String>,
        client_id: impl Into<String>,
        client_secret: impl Into<String>,
    ) -> Self {
        let verifier = JwtVerifier::new(Arc::clone(&transport));
        Self {
            transport,
            tenant_id: tenant_id.into(),
            client_id: client_id.into(),
            client_secret: client_secret.into(),
            login_base: TEAMS_LOGIN_BASE.to_string(),
            token: Mutex::new(None),
            service_url: DEFAULT_SERVICE_URL.to_string(),
            verifier,
        }
    }

    /// Point the token endpoint at a sovereign cloud or a test double; a trailing `/` is dropped.
    pub fn with_login_base(mut self, base: impl Into<String>) -> Self {
        self.login_base = base.into().trim_end_matches('/').to_string();
        self
    }

    /// Set the default Bot Framework connector base used when a send carries no
    /// `metadata["service_url"]`; a trailing `/` is dropped, and an empty value keeps
    /// [`DEFAULT_SERVICE_URL`].
    ///
    /// Regional and government bots need the `serviceUrl` their own activities arrived with, so the
    /// per-send override is usually the better mechanism — see [`TeamsAdapter::send`].
    pub fn with_service_url(mut self, url: impl Into<String>) -> Self {
        let trimmed = url.into();
        let trimmed = trimmed.trim().trim_end_matches('/');
        if !trimmed.is_empty() {
            self.service_url = trimmed.to_string();
        }
        self
    }

    /// The current Bot Framework bearer token: the cached one while it is still live, otherwise a
    /// fresh one from the client-credentials endpoint.
    ///
    /// Single-flight is not attempted (two concurrent misses may both fetch); the [`Mutex`] only
    /// guards the cache slot, which is what keeps later calls from re-fetching.
    pub async fn access_token(&self) -> Result<String, String> {
        self.acquire_token()
            .await
            .map_err(|failure| failure.message)
    }

    /// Whether `authorization` proves the activity really came from the Bot Framework on behalf of
    /// this bot.
    ///
    /// Strips the `Bearer ` scheme, then verifies the token against [`BOT_FRAMEWORK_JWKS_URL`] with
    /// `iss` = [`BOT_FRAMEWORK_ISSUER`] and `aud` = the adapter's `client_id` (the Microsoft app id
    /// the Bot Framework addresses activities to). Every failure — an empty `client_id`, no header,
    /// a malformed token, a bad signature, an expired token, the wrong issuer or audience, an
    /// unreachable JWKS — is `false`: an inbound webhook may only ever fail closed.
    ///
    /// The `serviceUrl` claim is **not** checked against the activity's own `serviceUrl` here; the
    /// host must still refuse to reply to a connector base that the verified activity did not carry
    /// (see the module's first trust note and [`TeamsAdapter::parse_webhook`]).
    pub async fn verify_request(&self, authorization: &str) -> bool {
        let client_id = self.client_id.trim();
        if client_id.is_empty() {
            return false;
        }
        let Some(token) = bearer_token(authorization) else {
            return false;
        };
        self.verifier
            .verify(
                token,
                BOT_FRAMEWORK_JWKS_URL,
                BOT_FRAMEWORK_ISSUER,
                client_id,
            )
            .await
            .is_ok()
    }

    /// Normalize one Bot Framework activity into events.
    ///
    /// An activity becomes a [`MessageEvent`] only when `type == "message"` and `text` is present and
    /// non-blank; `conversationUpdate`, `typing`, `invoke`, `messageReaction`, `event` and every other
    /// activity kind are skipped (an empty vector, not an error), as is a message activity without a
    /// `conversation.id`. Invalid JSON is an `Err`, because that means the host is not forwarding Bot
    /// Framework payloads at all.
    ///
    /// Mapping (hermes `_on_message`, `_CHAT_TYPES`):
    /// - `chat_id` = `conversation.id`; `chat_name` = `conversation.name`;
    /// - `chat_type`: `personal` ⇒ `"dm"`, `channel`/`groupChat` ⇒ `"group"`, anything else ⇒ `"dm"`;
    /// - author: `from.aadObjectId` (an AAD object id) else `from.id`, plus `from.name`;
    /// - tenant: `conversation.tenantId` mirrored onto `scope_id`/`guild_id`;
    /// - `message_id` = activity `id`, `reply_to_message_id` = `replyToId`,
    ///   `platform_update_id` = `timestamp` in epoch milliseconds;
    /// - `metadata["service_url"]` carries the activity's `serviceUrl`, so a reply can be routed back
    ///   without the adapter keeping conversation state.
    ///
    /// **The activity is not authenticated here.** Call [`TeamsAdapter::verify_request`] on the
    /// request's `Authorization` header first — see the module documentation.
    pub fn parse_webhook(&self, body: &str) -> Result<Vec<MessageEvent>, String> {
        let root: Value =
            serde_json::from_str(body).map_err(|err| format!("invalid json: {err}"))?;
        match activity_event(&root) {
            Some(event) => Ok(vec![event]),
            None => Ok(Vec::new()),
        }
    }

    /// `{login_base}/{tenant}/oauth2/v2.0/token`.
    fn token_url(&self) -> String {
        format!(
            "{}/{}/oauth2/v2.0/token",
            self.login_base,
            encode_path_segment(&self.tenant_id)
        )
    }

    /// The connector base for one send: `metadata["service_url"]` when it carries a non-blank
    /// string, else the configured default.
    ///
    /// The override is *trusted input*; see the module's trust note.
    fn resolve_service_url(&self, metadata: Option<&Value>) -> String {
        let override_url = metadata
            .and_then(|metadata| metadata.get("service_url"))
            .and_then(Value::as_str)
            .map(str::trim)
            .filter(|url| !url.is_empty());
        match override_url {
            Some(url) => url.trim_end_matches('/').to_string(),
            None => self.service_url.clone(),
        }
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

    /// Cache `value` for `ttl_secs`; a zero TTL stores nothing, so a token whose lifetime is inside
    /// [`TOKEN_SAFETY_MARGIN_SECS`] is re-fetched next time instead of being handed out already stale.
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
        self.fetch_access_token().await
    }

    /// `POST {login_base}/{tenant}/oauth2/v2.0/token`, caching the token on a 200 carrying
    /// `access_token`.
    async fn fetch_access_token(&self) -> Result<String, Failure> {
        // Field order matches hermes' `_bf_token_request` form, so the wire body is identical.
        let pairs = [
            ("grant_type", "client_credentials".to_string()),
            ("scope", BOT_FRAMEWORK_SCOPE.to_string()),
            ("client_id", self.client_id.clone()),
            ("client_secret", self.client_secret.clone()),
        ];
        let request = HttpRequest::form(self.token_url(), &pairs);
        let response = self
            .transport
            .execute(request)
            .await
            .map_err(|err| Failure::transient(format!("teams token request failed: {err}")))?;
        let parsed = response.json().unwrap_or(Value::Null);
        if !response.ok() {
            return Err(credentials_failure(response.status, &parsed));
        }
        let token = parsed
            .get("access_token")
            .and_then(Value::as_str)
            .map(str::trim)
            .filter(|token| !token.is_empty());
        let Some(token) = token else {
            return Err(Failure::unknown(
                "teams token response carried no access_token",
            ));
        };
        let expires_in = parsed
            .get("expires_in")
            .and_then(value_to_i64)
            .unwrap_or(DEFAULT_TOKEN_TTL_SECS);
        let ttl_secs = expires_in.saturating_sub(TOKEN_SAFETY_MARGIN_SECS);
        if ttl_secs > 0 {
            self.store_token(token, ttl_secs as u64);
        }
        Ok(token.to_string())
    }

    /// One chunk through `POST …/conversations/{id}/activities`, returning the new activity `id`.
    async fn post_message(
        &self,
        service_url: &str,
        chat_id: &str,
        chunk: &str,
        reply_to: Option<&str>,
    ) -> Result<String, Failure> {
        let token = self.acquire_token().await?;
        let mut payload = json!({ "type": "message", "text": chunk });
        // Bot Framework threads an activity under another one through `replyToId`; hermes does the
        // same through `App.reply()` (falling back to a flat send when a group chat rejects it, which
        // this port cannot distinguish — a rejected reply surfaces as the ordinary failure below).
        if let Some(reply_to) = reply_to.map(str::trim).filter(|id| !id.is_empty()) {
            payload["replyToId"] = Value::String(reply_to.to_string());
        }
        let request =
            HttpRequest::json(activities_url(service_url, chat_id), &payload).bearer(&token);
        let response = self
            .transport
            .execute(request)
            .await
            .map_err(|err| Failure::transient(format!("teams send transport error: {err}")))?;
        let parsed = response.json().unwrap_or(Value::Null);
        if !response.ok() {
            let failure = classify(response.status, &parsed, response.retry_after);
            if failure.token_invalid {
                self.invalidate_token();
            }
            return Err(failure);
        }
        match text_field(Some(&parsed), "id") {
            Some(activity_id) => Ok(activity_id),
            None => Err(Failure::unknown("teams activity response carried no id")),
        }
    }

    /// One pre-built activity through `POST …/conversations/{id}/activities`, returning the new
    /// activity `id`.
    ///
    /// Shares the text path's token flow and failure taxonomy; only the body differs (an
    /// `attachments` entry instead of plain `text`). `what` names the activity kind in the failure
    /// text (`"media"`, `"card"`) so a refusal says which send it came from.
    ///
    /// A 200 that carries no `id` is a failure: an activity the connector cannot name is one this
    /// adapter cannot later edit or delete, and reporting it as delivered would be a lie.
    async fn post_activity(
        &self,
        service_url: &str,
        chat_id: &str,
        payload: &Value,
        what: &str,
    ) -> Result<String, Failure> {
        let token = self.acquire_token().await?;
        let request =
            HttpRequest::json(activities_url(service_url, chat_id), payload).bearer(&token);
        let response = self.transport.execute(request).await.map_err(|err| {
            Failure::transient(format!("teams {what} send transport error: {err}"))
        })?;
        let parsed = response.json().unwrap_or(Value::Null);
        if !response.ok() {
            let failure = classify(response.status, &parsed, response.retry_after);
            if failure.token_invalid {
                self.invalidate_token();
            }
            return Err(failure);
        }
        match text_field(Some(&parsed), "id") {
            Some(activity_id) => Ok(activity_id),
            None => Err(Failure::unknown(format!(
                "teams {what} activity response carried no activity id"
            ))),
        }
    }
}

#[async_trait]
impl PlatformAdapter for TeamsAdapter {
    /// Mint a Bot Framework bearer token. True only when the client-credentials endpoint answers 200
    /// with an `access_token`, which is the only proof the Azure AD app identity is live. There is no
    /// long-lived connection to re-establish (the host owns the webhook listener), so `is_reconnect`
    /// needs no different handling.
    async fn connect(&mut self, _is_reconnect: bool) -> bool {
        self.acquire_token().await.is_ok()
    }

    /// Forget the cached bearer token. Nothing else is held client-side.
    async fn disconnect(&mut self) {
        self.invalidate_token();
    }

    /// Trim, chunk to [`TEAMS_MAX_MESSAGE_LENGTH`] code points and deliver the chunks in order.
    ///
    /// Each chunk becomes one `message` activity at `{service_url}/v3/conversations/{chat_id}/activities`,
    /// where `service_url` is `metadata["service_url"]` when present (so a reply goes back to the
    /// connector the activity came from) else the configured default. The conversation id is
    /// percent-encoded so it cannot re-shape the request path.
    ///
    /// A transport failure, or a status the taxonomy cannot salvage, aborts the loop; the ids that
    /// already landed are reported as `continuation_message_ids` so a retry sends only the tail.
    ///
    /// Blank content is refused with `bad_format` before any request: a message activity with no
    /// text is not something the connector can deliver, and silently succeeding would hide a broken
    /// reply.
    async fn send(
        &self,
        chat_id: &str,
        content: &str,
        reply_to: Option<&str>,
        metadata: Option<Value>,
    ) -> SendResult {
        let service_url = self.resolve_service_url(metadata.as_ref());
        let trimmed = content.trim();
        if trimmed.is_empty() {
            return SendResult::failed(
                "teams cannot send an empty text message",
                Some("bad_format"),
            );
        }
        let chunks = truncate_message(trimmed, TEAMS_MAX_MESSAGE_LENGTH, codepoint_len);
        if chunks.is_empty() {
            return SendResult::failed(
                "teams cannot send an empty text message",
                Some("bad_format"),
            );
        }
        let mut delivered: Vec<String> = Vec::with_capacity(chunks.len());
        for chunk in &chunks {
            match self
                .post_message(&service_url, chat_id, chunk, reply_to)
                .await
            {
                Ok(activity_id) => delivered.push(activity_id),
                Err(failure) => {
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
            None => SendResult::failed("teams delivered no chunks", Some("unknown")),
        }
    }

    /// No cheap endpoint: the Bot Framework connector exposes no "describe this conversation" read,
    /// and every Teams conversation id already carries the routing information the gateway needs. The
    /// answer is therefore `{name: chat_id, chat_type: "group"}`, and no request is made.
    async fn get_chat_info(&self, chat_id: &str) -> ChatInfo {
        ChatInfo {
            name: chat_id.to_string(),
            chat_type: "group".to_string(),
        }
    }

    /// True: [`PlatformAdapter::send_media`] posts a real attachment activity, so a caller may hand
    /// this adapter bytes or a URL instead of falling back to a text link.
    fn supports_media(&self) -> bool {
        true
    }

    /// Send one attachment as a Teams `message` activity (hermes `_send_media_attachment`,
    /// adapter.py:745-773).
    ///
    /// The wire shape is the reference's, one activity per attachment:
    ///
    /// ```json
    /// {"type": "message",
    ///  "text": "<caption, only when non-blank>",
    ///  "replyToId": "<reply_to, only when given>",
    ///  "attachments": [{"contentType": "<mime>", "contentUrl": "<url>"}]}
    /// ```
    ///
    /// posted to `{service_url}/v3/conversations/{chat_id}/activities`, where `service_url` is the
    /// adapter's configured default ([`DEFAULT_SERVICE_URL`] unless
    /// [`TeamsAdapter::with_service_url`] said otherwise) — the activity update and delete paths do
    /// the same, because the trait gives `send_media` no per-send `metadata` to carry an activity's
    /// own `serviceUrl`.
    ///
    /// Content URL, exactly as the reference decides it:
    /// - [`MediaSource::Url`] is attached *by reference*: Teams fetches it itself. Only `http`/`https`
    ///   are accepted — any other scheme (`file:`, `data:`, `ftp:`, a bare path) is refused with zero
    ///   requests, because the connector would either reject it or, worse, treat it as a link the
    ///   user can be tricked into opening;
    /// - [`MediaSource::Bytes`] becomes `data:<mime>;base64,<payload>` (adapter.py:765). The URI only
    ///   ever interpolates the resolved MIME type and standard base64, both of which are safe in a
    ///   URL by construction, so nothing else needs escaping.
    ///
    /// The MIME type is `media.mime` when it is a bare `type/subtype`, else the per-kind default the
    /// reference uses (`image/png`, `video/mp4`, `audio/mpeg`, `application/octet-stream`). A value
    /// carrying parameters or junk falls back rather than being sanitised into a content URL.
    ///
    /// Refusals happen before the network is touched: empty bytes and a blank/non-http URL are
    /// `bad_format`, bytes over [`TEAMS_MAX_ATTACHMENT_BYTES`] are `too_long`. A blank caption is
    /// omitted (not sent as `text: ""`) and a blank `reply_to` is omitted rather than sent as an
    /// empty `replyToId`. Like [`TeamsAdapter::post_message`], a conversation that rejects a threaded
    /// activity (group chats 400 on `replyToId`) surfaces as the ordinary failure — this port has no
    /// way to tell "rejected the thread" from "rejected the media", so it never silently re-sends
    /// un-threaded.
    ///
    /// Success requires the connector to return an activity `id`; every failure — a bad token, 429,
    /// 401/403, 404, 413, 400, 5xx, a transport error, a 200 with no id — is mapped by the adapter's
    /// existing taxonomy and reported as a failure, never as a delivered attachment.
    async fn send_media(
        &self,
        chat_id: &str,
        media: &OutboundMedia,
        reply_to: Option<&str>,
    ) -> SendResult {
        let (mime, content_url) = match attachment_content(media) {
            Ok(pair) => pair,
            Err(refusal) => return refusal.into_send_result(),
        };
        let service_url = self.resolve_service_url(None);
        let payload = media_activity(&mime, &content_url, media.caption.as_deref(), reply_to);
        match self
            .post_activity(&service_url, chat_id, &payload, "media")
            .await
        {
            Ok(activity_id) => SendResult::ok(activity_id),
            Err(failure) => failure.into_send_result(),
        }
    }

    /// True: [`PlatformAdapter::send_card`] posts a real adaptive-card attachment activity, so a
    /// caller may hand this adapter an Adaptive Card instead of flattening it into text.
    fn supports_cards(&self) -> bool {
        true
    }

    /// Send one Adaptive Card as a Teams `message` activity carrying a single adaptive-card
    /// attachment.
    ///
    /// The wire shape follows the reference's `_send_card` (adapter.py:604-609), which hands the
    /// SDK a typed `AdaptiveCard` model:
    /// `MessageActivityInput().add_card(card)` becomes one `attachments` entry whose `contentType`
    /// is [`TEAMS_ADAPTIVE_CARD_CONTENT_TYPE`] and whose `content` is the card object itself. On the
    /// connector that is
    ///
    /// ```json
    /// {"type": "message",
    ///  "attachments": [{"contentType": "application/vnd.microsoft.card.adaptive",
    ///                   "content": {"type": "AdaptiveCard", "version": "1.4", "body": […]}}]}
    /// ```
    ///
    /// posted to `{service_url}/v3/conversations/{chat_id}/activities`, where `service_url` is
    /// [`TeamsAdapter::resolve_service_url`]`(None)` — i.e. the configured default
    /// ([`DEFAULT_SERVICE_URL`] unless [`TeamsAdapter::with_service_url`] said otherwise), because
    /// the trait's `send_card` carries no per-send `metadata` to route a per-conversation connector
    /// base. Success requires the connector to return an activity `id`; a 2xx without one is
    /// reported as `unknown`, never as a delivered card. Every other failure (bad token, 429,
    /// 401/403, 404, 413, 400, 5xx, a transport error) goes through the adapter's existing
    /// [`classify`] taxonomy — the same one text and media use — and a 401 drops the cached token.
    ///
    /// Validation happens before any request. The card must be a **JSON object** carrying
    /// [`TEAMS_ADAPTIVE_CARD_TYPE`] as its `type`: the reference always sends an SDK-typed card, so
    /// a type-less or wrongly-typed object is refused with `bad_format` rather than retyped here —
    /// guessing would let a `MessageCard` (a different `contentType`) be labelled adaptive and
    /// render as an empty attachment. A non-object (string, array, number, null) is refused the
    /// same way. `version` is not enforced: Bot Framework renders a card without one poorly but
    /// accepts the activity, and the reference pins `1.4` on the models it builds (`AdaptiveCard()
    /// .with_version("1.4")`, adapter.py:709).
    ///
    /// The card body is passed through byte-for-byte otherwise: this adapter never rewrites the
    /// caller's `body`/`actions`, and an oversized card is left to the connector's own size limit
    /// (surfacing as `too_long`/`bad_format` from [`classify`]).
    async fn send_card(&self, chat_id: &str, card: &Value) -> SendResult {
        let content = match adaptive_card_content(card) {
            Ok(content) => content,
            Err(refusal) => return refusal.into_send_result(),
        };
        let service_url = self.resolve_service_url(None);
        let payload = card_activity(&content);
        match self
            .post_activity(&service_url, chat_id, &payload, "card")
            .await
        {
            Ok(activity_id) => SendResult::ok(activity_id),
            Err(failure) => failure.into_send_result(),
        }
    }

    /// `send` chunks natively through [`truncate_message`]; the delivery router must not truncate
    /// first.
    fn splits_long_messages(&self) -> bool {
        true
    }

    /// The per-message cap `send` chunks against, so the router and the sender agree.
    fn max_message_length(&self) -> usize {
        TEAMS_MAX_MESSAGE_LENGTH
    }

    /// Teams (like the Bot Framework text field) counts code points, which is already the trait
    /// default; stated explicitly so the cap and the chunker agree on the unit.
    fn message_length(&self, text: &str) -> usize {
        codepoint_len(text)
    }

    /// Best-effort `{"type": "typing"}` activity (hermes `send_typing`).
    ///
    /// Every failure is swallowed: a typing indicator the user never sees is not worth failing a turn
    /// for, and Teams expires it on its own — there is no "stop typing" activity, so
    /// [`PlatformAdapter::stop_typing`] stays the default no-op.
    async fn send_typing(&self, chat_id: &str, metadata: Option<Value>) {
        let service_url = self.resolve_service_url(metadata.as_ref());
        let Ok(token) = self.acquire_token().await else {
            return;
        };
        let request = HttpRequest::json(
            activities_url(&service_url, chat_id),
            &json!({"type": "typing"}),
        )
        .bearer(&token);
        let _ = self.transport.execute(request).await;
    }

    /// Update an already-sent activity: `PUT {service_url}/v3/conversations/{chat_id}/activities/{id}`
    /// with `{"type": "message", "text": content}`.
    ///
    /// Teams really does support editing a bot's own message this way, so unlike the Feishu adapter
    /// this is an honest implementation rather than a refusal. The trait gives no `metadata`, so the
    /// update goes to the configured [`DEFAULT_SERVICE_URL`] (or the [`with_service_url`] override),
    /// not to a per-conversation connector base.
    ///
    /// Failures use the same taxonomy as [`PlatformAdapter::send`]; `finalize` has no meaning here
    /// (Teams treats every update as final) and is ignored.
    async fn edit_message(
        &self,
        chat_id: &str,
        message_id: &str,
        content: &str,
        _finalize: bool,
    ) -> SendResult {
        let trimmed = content.trim();
        if trimmed.is_empty() {
            return SendResult::failed(
                "teams cannot update a message to empty text",
                Some("bad_format"),
            );
        }
        let token = match self.acquire_token().await {
            Ok(token) => token,
            Err(failure) => return failure.into_send_result(),
        };
        let payload = json!({ "type": "message", "text": trimmed });
        let request = HttpRequest::put_json(
            activity_url(&self.service_url, chat_id, message_id),
            &payload,
        )
        .bearer(&token);
        let response = match self.transport.execute(request).await {
            Ok(response) => response,
            Err(err) => {
                return Failure::transient(format!("teams edit transport error: {err}"))
                    .into_send_result();
            }
        };
        let parsed = response.json().unwrap_or(Value::Null);
        if !response.ok() {
            let failure = classify(response.status, &parsed, response.retry_after);
            if failure.token_invalid {
                self.invalidate_token();
            }
            return failure.into_send_result();
        }
        // The connector echoes the updated activity; the id is stable, so fall back to the one we
        // asked about rather than reporting "no id" for a successful edit.
        let activity_id = text_field(Some(&parsed), "id").unwrap_or_else(|| message_id.to_string());
        SendResult::ok(activity_id)
    }

    /// `DELETE {service_url}/v3/conversations/{chat_id}/activities/{id}`.
    ///
    /// The trait returns `()` and callers treat deletion as best effort (ephemeral replies), so the
    /// request is issued and every outcome — including a transport error — is swallowed. The
    /// configured connector base is used for the same reason as in
    /// [`PlatformAdapter::edit_message`].
    async fn delete_message(&self, chat_id: &str, message_id: &str) {
        let Ok(token) = self.acquire_token().await else {
            return;
        };
        let request = HttpRequest::delete(activity_url(&self.service_url, chat_id, message_id))
            .bearer(&token);
        let _ = self.transport.execute(request).await;
    }
}

/// How one Bot Framework call failed, carrying everything the send taxonomy needs.
#[derive(Debug)]
struct Failure {
    message: String,
    kind: &'static str,
    retryable: bool,
    retry_after: Option<f64>,
    /// The status said the bearer token is worthless, so the cache entry is dropped.
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

/// The Bot Framework failure taxonomy.
///
/// - 429 → `rate_limited`, retryable, honouring `Retry-After`;
/// - 401/403 → `forbidden`; a 401 also drops the cached token, because the connector just said the
///   bearer is invalid (403 means the bot lacks the permission, which a new token cannot fix);
/// - 404 → `not_found` (the conversation or activity is gone);
/// - 413 → `too_long`: the connector refused the payload's size, which no retry can change;
/// - 400 and every other 4xx → `bad_format`: the payload, not the network, was wrong;
/// - 5xx and anything unclassified (1xx/3xx, or a 2xx that still failed higher up) → `transient`,
///   retryable: nothing was delivered, so a retry is the honest default.
fn classify(status: u16, body: &Value, retry_after: Option<f64>) -> Failure {
    let kind = match status {
        429 => "rate_limited",
        401 | 403 => "forbidden",
        404 => "not_found",
        413 => "too_long",
        400..=499 => "bad_format",
        _ => "transient",
    };
    let retryable = matches!(kind, "rate_limited" | "transient");
    Failure {
        message: render_error(status, body),
        kind,
        retryable,
        retry_after: if retryable { retry_after } else { None },
        token_invalid: status == 401,
    }
}

/// A rejected app identity at the token endpoint. There a 400/401/403 can only mean the
/// client id/secret/tenant were refused, so it is reported as `forbidden` (non-retryable) rather than
/// the `bad_format` the send taxonomy would pick for a 4xx.
fn credentials_failure(status: u16, body: &Value) -> Failure {
    let mut failure = classify(status, body, None);
    if matches!(status, 400 | 401 | 403) {
        failure.kind = "forbidden";
        failure.retryable = false;
        failure.retry_after = None;
        // Nothing is cached yet, and a credential refusal is not an expired token.
        failure.token_invalid = false;
    }
    failure
}

/// Human-readable error text. Bot Framework wraps its errors in `{"error": {"code", "message"}}`;
/// a bare `{"message": …}` (Azure AD's shape) and a status-only body are handled too.
fn render_error(status: u16, body: &Value) -> String {
    let nested = body.get("error");
    let code = text_field(nested, "code");
    let message = text_field(nested, "message").or_else(|| text_field(Some(body), "message"));
    match (code, message) {
        (Some(code), Some(message)) => format!("teams error {code} (HTTP {status}): {message}"),
        (Some(code), None) => format!("teams error {code} (HTTP {status})"),
        (None, Some(message)) => format!("teams HTTP {status}: {message}"),
        (None, None) => format!("teams HTTP {status}"),
    }
}

/// `{service_url}/v3/conversations/{chat_id}/activities`.
fn activities_url(service_url: &str, chat_id: &str) -> String {
    format!(
        "{}/v3/conversations/{}/activities",
        service_url.trim_end_matches('/'),
        encode_path_segment(chat_id)
    )
}

/// `{service_url}/v3/conversations/{chat_id}/activities/{activity_id}`.
fn activity_url(service_url: &str, chat_id: &str, activity_id: &str) -> String {
    format!(
        "{}/v3/conversations/{}/activities/{}",
        service_url.trim_end_matches('/'),
        encode_path_segment(chat_id),
        encode_path_segment(activity_id)
    )
}

/// Percent-encode one URL path segment (unreserved characters kept, everything else `%XX`) so a
/// conversation or activity id can never re-shape the request path. Teams ids look like
/// `19:abc@thread.skype;messageid=123`, so the `:`/`@`/`;`/`=` must be escaped.
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

// ----- outbound media (hermes `_send_media_attachment`) and adaptive cards (`_send_card`) -----

/// A refusal this adapter decides locally, before any request: the text to report plus the
/// [`crate::platform::SEND_ERROR_KINDS`] kind it belongs to.
///
/// Raised by the media path ([`attachment_content`]) and the card path
/// ([`adaptive_card_content`]): both only refuse what this adapter already knows it cannot deliver,
/// so nothing is requested on their behalf.
///
/// Kept as its own shape (rather than a [`SendResult`]) so the refusal path never builds a whole
/// result just to report why it stopped.
struct Refusal {
    message: String,
    kind: &'static str,
}

impl Refusal {
    fn new(kind: &'static str, message: impl Into<String>) -> Self {
        Self {
            message: message.into(),
            kind,
        }
    }

    fn into_send_result(self) -> SendResult {
        SendResult::failed(self.message, Some(self.kind))
    }
}

/// The `(contentType, contentUrl)` pair one outbound attachment becomes, or the refusal to report.
///
/// A URL is attached by reference; held bytes become a `data:<mime>;base64,…` URL
/// (adapter.py:758-766). Both refusals here are local: no request is made for media this adapter
/// already knows it cannot deliver.
fn attachment_content(media: &OutboundMedia) -> Result<(String, String), Refusal> {
    let mime = media_mime(media);
    match &media.source {
        MediaSource::Url(url) => {
            let url = url.trim();
            if url.is_empty() {
                return Err(Refusal::new(
                    "bad_format",
                    "teams cannot attach an empty media URL",
                ));
            }
            if !is_http_url(url) {
                return Err(Refusal::new(
                    "bad_format",
                    format!(
                        "teams attaches media by reference, so the content URL must be http(s): \
                         refusing {url:?} rather than guessing how the connector would treat it"
                    ),
                ));
            }
            Ok((mime, url.to_string()))
        }
        MediaSource::Bytes(bytes) => {
            if bytes.is_empty() {
                return Err(Refusal::new(
                    "bad_format",
                    "teams cannot attach empty media bytes",
                ));
            }
            if bytes.len() > TEAMS_MAX_ATTACHMENT_BYTES {
                return Err(Refusal::new(
                    "too_long",
                    format!(
                        "teams attachment is {} bytes, over the {} byte limit",
                        bytes.len(),
                        TEAMS_MAX_ATTACHMENT_BYTES
                    ),
                ));
            }
            Ok((mime.clone(), data_uri(&mime, bytes)))
        }
    }
}

/// One media activity body: `type`, the optional `text` caption, the optional `replyToId` and the
/// single `attachments` entry (adapter.py:766-768).
///
/// A caption is only sent when it is non-blank: Teams renders an empty `text` on an attachment
/// activity as an empty line above the file, so omitting the key is the honest encoding of "no
/// caption". `reply_to` follows the same rule as the text path: a blank anchor is not a thread.
fn media_activity(
    mime: &str,
    content_url: &str,
    caption: Option<&str>,
    reply_to: Option<&str>,
) -> Value {
    let mut payload = json!({
        "type": "message",
        "attachments": [{ "contentType": mime, "contentUrl": content_url }],
    });
    if let Some(caption) = caption.map(str::trim).filter(|caption| !caption.is_empty()) {
        payload["text"] = Value::String(caption.to_string());
    }
    if let Some(reply_to) = reply_to.map(str::trim).filter(|id| !id.is_empty()) {
        payload["replyToId"] = Value::String(reply_to.to_string());
    }
    payload
}

/// Validate one caller-supplied Adaptive Card and return the `content` of its attachment, or the
/// refusal to report.
///
/// The reference never builds a card from raw JSON: it composes an SDK `AdaptiveCard`
/// (adapter.py:604-609, 633, 709) whose model is always typed [`TEAMS_ADAPTIVE_CARD_TYPE`] and
/// versioned. This port receives JSON, so the two things the SDK guaranteed are checked here
/// instead, and both refusals are local `bad_format`s (no request is made):
/// - not a JSON object — an array, string, number or `null` has no card to attach;
/// - an empty object, or one whose `type` is missing or is not `AdaptiveCard` — sending it under
///   [`TEAMS_ADAPTIVE_CARD_CONTENT_TYPE`] would claim a card type the payload does not declare, so
///   it is refused with an actionable message rather than retyped by guesswork.
///
/// Everything else (`version`, `body`, `actions`, `$schema`, unknown members) is passed through
/// untouched: those are the caller's card, not this adapter's.
fn adaptive_card_content(card: &Value) -> Result<Value, Refusal> {
    let Some(object) = card.as_object() else {
        return Err(Refusal::new(
            "bad_format",
            format!(
                "teams sends an Adaptive Card as a JSON object, not {}",
                json_type_name(card)
            ),
        ));
    };
    if object.is_empty() {
        return Err(Refusal::new(
            "bad_format",
            "teams cannot send an empty Adaptive Card object",
        ));
    }
    match object
        .get("type")
        .and_then(Value::as_str)
        .map(str::trim)
        .filter(|card_type| !card_type.is_empty())
    {
        Some(card_type) if card_type.eq_ignore_ascii_case(TEAMS_ADAPTIVE_CARD_TYPE) => {
            Ok(card.clone())
        }
        Some(other) => Err(Refusal::new(
            "bad_format",
            format!(
                "teams sends Adaptive Cards under {TEAMS_ADAPTIVE_CARD_CONTENT_TYPE}, so the card's \
                 \"type\" must be \"{TEAMS_ADAPTIVE_CARD_TYPE}\", not {other:?}"
            ),
        )),
        None => Err(Refusal::new(
            "bad_format",
            format!(
                "teams refuses an Adaptive Card with no \"type\": an Adaptive Card must declare \
                 \"type\": \"{TEAMS_ADAPTIVE_CARD_TYPE}\" (the reference always sends a typed SDK \
                 card, adapter.py:709)"
            ),
        )),
    }
}

/// One adaptive-card activity body: `type`, and the single `attachments` entry carrying the card.
///
/// The shape `MessageActivityInput().add_card(card)` serializes (adapter.py:604-609): no `text`,
/// because a card activity carries its content in the attachment and an empty `text` would render
/// as a blank line above the card.
fn card_activity(card: &Value) -> Value {
    json!({
        "type": "message",
        "attachments": [{
            "contentType": TEAMS_ADAPTIVE_CARD_CONTENT_TYPE,
            "content": card,
        }],
    })
}

/// The JSON type of `value`, for a refusal that says what arrived ("an array") instead of dumping
/// the payload.
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

/// The MIME type one attachment declares: `media.mime` when it is a bare `type/subtype`, else the
/// per-kind default the reference passes for that send.
fn media_mime(media: &OutboundMedia) -> String {
    media
        .mime
        .as_deref()
        .map(str::trim)
        .filter(|mime| is_media_type(mime))
        .map(str::to_ascii_lowercase)
        .unwrap_or_else(|| default_media_mime(media.kind).to_string())
}

/// The reference's per-kind default MIME: `send_image` `image/png`, `send_video` `video/mp4`,
/// `send_voice` `audio/mpeg`, `send_document` `application/octet-stream` (adapter.py:775-794).
fn default_media_mime(kind: MediaKind) -> &'static str {
    match kind {
        MediaKind::Image => "image/png",
        MediaKind::Video => "video/mp4",
        MediaKind::Audio => "audio/mpeg",
        MediaKind::Document => "application/octet-stream",
    }
}

/// Whether `value` is a bare media type: exactly one `/`, both halves non-empty, every character an
/// RFC 6838 token character.
///
/// Parameters (`; charset=…`), whitespace and anything else a caller might smuggle in are refused,
/// because the value is interpolated verbatim into both `contentType` and a `data:` URI — the only
/// two things this adapter lets into the URI are the MIME type and the base64 payload.
fn is_media_type(value: &str) -> bool {
    let Some((type_part, subtype)) = value.split_once('/') else {
        return false;
    };
    !type_part.is_empty()
        && !subtype.is_empty()
        && !subtype.contains('/')
        && value.chars().all(|ch| {
            ch.is_ascii_alphanumeric()
                || matches!(
                    ch,
                    '!' | '#' | '$' | '&' | '^' | '_' | '.' | '+' | '-' | '/'
                )
        })
}

/// Whether `url` is an absolute `http`/`https` URL with a non-empty authority, i.e. something the
/// connector can fetch itself. The scheme is matched case-insensitively (RFC 3986 makes it
/// case-insensitive; the reference's `startswith` is stricter, and being lenient here only ever
/// accepts a URL Teams would also accept).
fn is_http_url(url: &str) -> bool {
    let Some((scheme, rest)) = url.split_once("://") else {
        return false;
    };
    if !matches!(scheme.to_ascii_lowercase().as_str(), "http" | "https") {
        return false;
    }
    let authority = rest
        .split(['/', '?', '#'])
        .next()
        .unwrap_or_default()
        .trim();
    !authority.is_empty()
}

/// `data:<mime>;base64,<payload>` — the content URL Teams accepts in place of a fetchable address
/// (adapter.py:765). The MIME type is already known to be a bare media type (see
/// [`is_media_type`]) and base64's alphabet (`A-Za-z0-9+/=`) is URL-safe by construction, so nothing
/// needs percent-encoding.
fn data_uri(mime: &str, bytes: &[u8]) -> String {
    format!("data:{mime};base64,{}", base64_encode(bytes))
}

/// Standard-alphabet Base64 with `=` padding (RFC 4648 §4), for the `data:` content URL.
///
/// Implemented here because the gateway crate may not add dependencies; the `wecom`/`weixin`/`sms`
/// adapters carry the same private encoder for the same reason.
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

/// The gateway's platform handle for this adapter. The constant is a fixed non-empty name, so the
/// fallback arm is unreachable and exists only to keep this function panic-free.
fn teams_platform() -> Platform {
    match Platform::new(TEAMS_PLATFORM_NAME) {
        Some(platform) => platform,
        None => Platform::local(),
    }
}

/// A JSON value as an `i64`, accepting the numeric-string form Azure AD uses for `expires_in`.
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

/// Build the normalized [`MessageEvent`] from one Bot Framework activity.
///
/// Returns `None` for anything that is not a text-bearing `message` activity with a conversation
/// (see [`TeamsAdapter::parse_webhook`]) so the caller skips it instead of inventing an event.
fn activity_event(root: &Value) -> Option<MessageEvent> {
    if root.get("type").and_then(Value::as_str) != Some("message") {
        return None;
    }
    let text = text_field(Some(root), "text")?;
    let conversation = root.get("conversation")?;
    let chat_id = text_field(Some(conversation), "id")?;

    // `personal` is the bot's one-to-one chat; `channel` (a team channel) and `groupChat` are both
    // multi-user conversations. hermes maps an unknown/missing type to `dm` (`_CHAT_TYPES.get(…) or
    // "dm"`); `channel` collapses onto `group` here because the gateway has no channel chat type.
    let chat_type = match text_field(Some(conversation), "conversationType").as_deref() {
        Some("channel" | "groupChat") => "group",
        _ => "dm",
    };

    let from = root.get("from");
    let user_id = text_field(from, "aadObjectId").or_else(|| text_field(from, "id"));
    let user_name = text_field(from, "name");
    let message_id = text_field(Some(root), "id");

    let mut source = SessionSource::new(teams_platform(), chat_id);
    source.chat_type = chat_type.to_string();
    source.chat_name = text_field(Some(conversation), "name");
    source.user_id = user_id.clone();
    source.user_name = user_name.clone();
    source.message_id = message_id.clone();
    source.guild_id = text_field(Some(conversation), "tenantId");
    source.normalize_scope();

    let mut parsed = MessageEvent::new(text, source);
    parsed.user_id = user_id;
    parsed.user_name = user_name;
    parsed.message_id = message_id;
    parsed.platform_update_id = root
        .get("timestamp")
        .and_then(Value::as_str)
        .and_then(parse_timestamp_millis);
    parsed.reply_to_message_id = text_field(Some(root), "replyToId");
    if let Some(service_url) = text_field(Some(root), "serviceUrl") {
        // Carried on the event so a reply can be addressed to the connector this activity arrived
        // from, without the adapter having to remember a conversation reference.
        parsed
            .metadata
            .insert("service_url".to_string(), Value::String(service_url));
    }
    parsed.raw_message = Some(root.clone());
    Some(parsed)
}

/// Bot Framework activity timestamps in epoch milliseconds, or `None` when unparseable.
///
/// ISO 8601 / RFC 3339 as Teams sends it: `2024-05-01T12:00:00.000Z`, with an optional fraction and
/// either a `Z` or a `±HH:MM` / `±HHMM` offset. Hand-rolled because the gateway crate has no date
/// dependency and none may be added.
fn parse_timestamp_millis(raw: &str) -> Option<i64> {
    let (date_part, rest) = raw.trim().split_once('T')?;
    let mut date_fields = date_part.split('-');
    let year: i64 = date_fields.next()?.parse().ok()?;
    let month: i64 = date_fields.next()?.parse().ok()?;
    let day: i64 = date_fields.next()?.parse().ok()?;
    if date_fields.next().is_some() {
        return None;
    }

    let (time_part, offset_secs) = split_zone(rest)?;
    let mut time_fields = time_part.split(':');
    let hour: i64 = time_fields.next()?.parse().ok()?;
    let minute: i64 = time_fields.next()?.parse().ok()?;
    let seconds_field = time_fields.next()?;
    if time_fields.next().is_some() {
        return None;
    }
    let (second_str, fraction) = match seconds_field.split_once('.') {
        Some((seconds, fraction)) => (seconds, Some(fraction)),
        None => (seconds_field, None),
    };
    let second: i64 = second_str.parse().ok()?;
    let millis = match fraction {
        Some(fraction) => fraction_millis(fraction)?,
        None => 0,
    };
    if !(1..=12).contains(&month)
        || !(1..=31).contains(&day)
        || !(0..=23).contains(&hour)
        || !(0..=59).contains(&minute)
        || !(0..=60).contains(&second)
    {
        return None;
    }
    let days = days_from_civil(year, month, day);
    let seconds = days * 86_400 + hour * 3600 + minute * 60 + second - offset_secs;
    seconds.checked_mul(1000)?.checked_add(millis)
}

/// Split `HH:MM:SS[.frac]<zone>` into the time part and the zone offset in seconds east of UTC.
fn split_zone(rest: &str) -> Option<(&str, i64)> {
    if let Some(time_part) = rest.strip_suffix(['Z', 'z']) {
        return Some((time_part, 0));
    }
    let idx = rest.rfind(['+', '-'])?;
    let (time_part, zone) = rest.split_at(idx);
    let sign: i64 = if zone.starts_with('-') { -1 } else { 1 };
    let zone = &zone[1..];
    let (hours_str, minutes_str) = match zone.split_once(':') {
        Some(parts) => parts,
        None if zone.len() == 4 => zone.split_at(2),
        None => return None,
    };
    let hours: i64 = hours_str.parse().ok()?;
    let minutes: i64 = minutes_str.parse().ok()?;
    if hours > 23 || minutes > 59 {
        return None;
    }
    Some((time_part, sign * (hours * 3600 + minutes * 60)))
}

/// The fractional second as milliseconds, zero-padded on the right (`"1"` ⇒ 100 ms).
fn fraction_millis(fraction: &str) -> Option<i64> {
    let digits: String = fraction.chars().take_while(char::is_ascii_digit).collect();
    if digits.is_empty() {
        return None;
    }
    let padded = format!("{digits:0<3}");
    padded.get(..3)?.parse().ok()
}

/// Days between 1970-01-01 and `year-month-day` (Howard Hinnant's `days_from_civil`), proleptic
/// Gregorian, so no timezone database is needed.
fn days_from_civil(year: i64, month: i64, day: i64) -> i64 {
    let year = if month <= 2 { year - 1 } else { year };
    let era = if year >= 0 { year } else { year - 399 } / 400;
    let year_of_era = year - era * 400;
    let month_prime = (month + 9) % 12;
    let day_of_year = (153 * month_prime + 2) / 5 + day - 1;
    let day_of_era = year_of_era * 365 + year_of_era / 4 - year_of_era / 100 + day_of_year;
    era * 146_097 + day_of_era - 719_468
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::jwt::test_support::{RSA_KID, rsa_jwks, sign_rs256};
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

        /// Requests that are not token fetches, i.e. the actual connector calls.
        fn api_requests(&self) -> Vec<HttpRequest> {
            self.requests()
                .into_iter()
                .filter(|request| !request.url.contains("/oauth2/v2.0/token"))
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

    /// The `text` of a message activity request.
    fn request_text(request: &HttpRequest) -> String {
        request_body(request)
            .get("text")
            .and_then(Value::as_str)
            .unwrap_or_default()
            .to_string()
    }

    fn bearer(request: &HttpRequest) -> Option<String> {
        request
            .headers
            .iter()
            .find(|(name, _)| name == "Authorization")
            .map(|(_, value)| value.clone())
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

    fn token_ok(expires_in: i64) -> Result<HttpResponse, TransportError> {
        response(
            200,
            json!({ "token_type": "Bearer", "access_token": "bf-token", "expires_in": expires_in }),
        )
    }

    fn activity_ok(activity_id: &str) -> Result<HttpResponse, TransportError> {
        response(200, json!({ "id": activity_id }))
    }

    fn connector_error(
        status: u16,
        code: &str,
        message: &str,
    ) -> Result<HttpResponse, TransportError> {
        response(
            status,
            json!({ "error": { "code": code, "message": message } }),
        )
    }

    fn adapter(mock: &Arc<MockTransport>) -> TeamsAdapter {
        TeamsAdapter::new(
            mock.clone() as SharedTransport,
            "tenant-1",
            "client-1",
            "s3cret",
        )
    }

    const DEFAULT_ACTIVITIES_URL: &str = "https://smba.trafficmanager.net/teams/v3/conversations/19%3Achat%40thread.skype/activities";

    fn non_whitespace(text: &str) -> String {
        text.chars().filter(|ch| !ch.is_whitespace()).collect()
    }

    #[tokio::test]
    async fn connect_fetches_a_token_and_later_sends_reuse_it() {
        let mock = MockTransport::new(vec![token_ok(3600), activity_ok("a-1"), activity_ok("a-2")]);
        let mut client = adapter(&mock);

        assert!(client.connect(false).await);
        assert!(
            client
                .send("19:chat@thread.skype", "first", None, None)
                .await
                .success
        );
        assert!(
            client
                .send("19:chat@thread.skype", "second", None, None)
                .await
                .success
        );

        // Two sends, one token fetch: the cached token is reused.
        assert_eq!(mock.token_requests(), 1);
        assert_eq!(mock.api_requests().len(), 2);

        let requests = mock.requests();
        assert_eq!(requests[0].method, HttpMethod::Post);
        assert_eq!(
            requests[0].url,
            "https://login.microsoftonline.com/tenant-1/oauth2/v2.0/token"
        );
        assert!(
            requests[0]
                .headers
                .iter()
                .any(|(name, value)| name == "Content-Type"
                    && value == "application/x-www-form-urlencoded"),
            "the token request must be a form post"
        );
        let form =
            String::from_utf8(requests[0].body.clone().unwrap_or_default()).unwrap_or_default();
        assert_eq!(
            form,
            "grant_type=client_credentials&scope=https%3A%2F%2Fapi.botframework.com%2F.default\
             &client_id=client-1&client_secret=s3cret"
        );

        // Every connector call carries the Bot Framework bearer.
        for request in mock.api_requests() {
            assert_eq!(bearer(&request).as_deref(), Some("Bearer bf-token"));
        }
        // The public accessor reuses the same cache entry.
        assert_eq!(client.access_token().await.unwrap_or_default(), "bf-token");
        assert_eq!(mock.token_requests(), 1);
    }

    #[tokio::test]
    async fn a_token_closer_than_the_safety_margin_is_not_cached() {
        let mock = MockTransport::new(vec![token_ok(30), token_ok(3600)]);
        let client = adapter(&mock);

        assert_eq!(client.access_token().await.unwrap_or_default(), "bf-token");
        // `expires_in` is inside the 60 s margin, so nothing was cached and the next call re-fetches.
        assert_eq!(client.access_token().await.unwrap_or_default(), "bf-token");
        assert_eq!(mock.token_requests(), 2);
    }

    #[tokio::test]
    async fn a_failed_token_request_fails_connect_and_the_send() {
        // A refused app identity.
        let rejected = MockTransport::new(vec![
            connector_error(
                401,
                "invalid_client_credentials",
                "AADSTS7000215: bad secret",
            ),
            connector_error(
                401,
                "invalid_client_credentials",
                "AADSTS7000215: bad secret",
            ),
        ]);
        let mut client = adapter(&rejected);
        assert!(!client.connect(false).await);
        let result = client
            .send("19:chat@thread.skype", "hello", None, None)
            .await;
        assert!(!result.success);
        assert_eq!(result.error_kind.as_deref(), Some("forbidden"));
        assert!(!result.retryable);
        assert!(
            result
                .error
                .as_deref()
                .unwrap_or_default()
                .contains("AADSTS7000215"),
            "{:?}",
            result.error
        );
        // Nothing was attempted against the connector.
        assert!(rejected.api_requests().is_empty());

        // An unreachable token endpoint is transient, not forbidden.
        let down = MockTransport::new(vec![Err(TransportError::Request("dns".to_string()))]);
        assert!(!adapter(&down).connect(false).await);

        // A 200 without an access_token is a failure too.
        let empty = MockTransport::new(vec![response(200, json!({ "token_type": "Bearer" }))]);
        assert!(!adapter(&empty).connect(false).await);
        assert!(empty.api_requests().is_empty());
    }

    #[tokio::test]
    async fn send_posts_a_message_activity_with_an_encoded_conversation_id() {
        let mock = MockTransport::new(vec![token_ok(3600), activity_ok("a-42")]);
        let result = adapter(&mock)
            .send("19:chat@thread.skype", "hello there", None, None)
            .await;

        assert!(result.success);
        assert_eq!(result.message_id.as_deref(), Some("a-42"));
        assert_eq!(result.continuation_message_ids, vec!["a-42"]);
        assert!(result.error.is_none());
        assert!(result.error_kind.is_none());

        let requests = mock.api_requests();
        assert_eq!(requests.len(), 1);
        assert_eq!(requests[0].method, HttpMethod::Post);
        assert_eq!(requests[0].url, DEFAULT_ACTIVITIES_URL);
        let body = request_body(&requests[0]);
        assert_eq!(body["type"], json!("message"));
        assert_eq!(body["text"], json!("hello there"));
        // No thread anchor was asked for, so none is sent.
        assert!(body.get("replyToId").is_none());
    }

    #[tokio::test]
    async fn reply_to_threads_the_activity_under_the_anchor() {
        let mock = MockTransport::new(vec![token_ok(3600), activity_ok("a-reply")]);
        let result = adapter(&mock)
            .send("19:chat@thread.skype", "answering", Some("a-parent"), None)
            .await;

        assert!(result.success);
        let body = request_body(&mock.api_requests()[0]);
        assert_eq!(body["type"], json!("message"));
        assert_eq!(body["replyToId"], json!("a-parent"));
    }

    #[tokio::test]
    async fn service_url_from_metadata_overrides_the_configured_default() {
        let mock = MockTransport::new(vec![token_ok(3600), activity_ok("a-gov")]);
        let metadata = json!({ "service_url": "https://smba.infra.gov.teams.microsoft.us/" });
        let result = adapter(&mock)
            .send("19:chat@thread.skype", "hello", None, Some(metadata))
            .await;

        assert!(result.success);
        assert_eq!(
            mock.api_requests()[0].url,
            "https://smba.infra.gov.teams.microsoft.us/v3/conversations/19%3Achat%40thread.skype/activities"
        );

        // A configured override is the fallback when the send carries none.
        let mock = MockTransport::new(vec![token_ok(3600), activity_ok("a-mirror")]);
        let client = adapter(&mock).with_service_url("https://mirror.test/connector/");
        assert!(
            client
                .send("19:chat@thread.skype", "hello", None, None)
                .await
                .success
        );
        assert_eq!(
            mock.api_requests()[0].url,
            "https://mirror.test/connector/v3/conversations/19%3Achat%40thread.skype/activities"
        );
        // A metadata value that is not a usable string falls back to the configured base.
        assert_eq!(
            client.resolve_service_url(Some(&json!({ "service_url": "   " }))),
            "https://mirror.test/connector"
        );
        assert_eq!(
            client.resolve_service_url(Some(&json!({}))),
            "https://mirror.test/connector"
        );
        assert_eq!(
            client.resolve_service_url(None),
            "https://mirror.test/connector"
        );
    }

    #[tokio::test]
    async fn long_content_is_chunked_within_the_code_point_cap() {
        let content: String = (0..500)
            .map(|idx| {
                format!(
                    "line {idx:03} of a deliberately long answer that the adapter has to chunk\n"
                )
            })
            .collect();
        assert!(
            content.chars().count() > TEAMS_MAX_MESSAGE_LENGTH,
            "test content must exceed one message"
        );

        let mut responses = vec![token_ok(3600)];
        responses.extend((0..8).map(|idx| activity_ok(&format!("a-{idx}"))));
        let mock = MockTransport::new(responses);
        let result = adapter(&mock)
            .send("19:chat@thread.skype", &content, None, None)
            .await;

        assert!(result.success, "{:?}", result.error);
        let requests = mock.api_requests();
        assert!(requests.len() > 1, "content should have been split");
        assert_eq!(mock.token_requests(), 1);
        for request in &requests {
            let text = request_text(request);
            assert!(
                !text.is_empty() && text.chars().count() <= TEAMS_MAX_MESSAGE_LENGTH,
                "chunk of {} code points exceeds the cap",
                text.chars().count()
            );
            assert_eq!(request.url, DEFAULT_ACTIVITIES_URL);
        }
        // Ids are reported in send order, with the last chunk's id as the message id.
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
    async fn connector_statuses_map_onto_the_send_taxonomy() {
        // (status, expected kind, retryable)
        let cases = [
            (429, "rate_limited", true),
            (401, "forbidden", false),
            (403, "forbidden", false),
            (404, "not_found", false),
            (400, "bad_format", false),
            (413, "too_long", false),
            (500, "transient", true),
            (503, "transient", true),
        ];
        for (status, kind, retryable) in cases {
            let mock = MockTransport::new(vec![
                token_ok(3600),
                connector_error(status, "BadArgument", "nope"),
            ]);
            let result = adapter(&mock)
                .send("19:chat@thread.skype", "hello", None, None)
                .await;
            assert!(!result.success, "HTTP {status} must fail");
            assert_eq!(result.error_kind.as_deref(), Some(kind), "HTTP {status}");
            assert_eq!(result.retryable, retryable, "HTTP {status}");
            assert!(
                result
                    .error
                    .as_deref()
                    .unwrap_or_default()
                    .contains("BadArgument"),
                "HTTP {status} lost the error detail: {:?}",
                result.error
            );
        }

        // 429 carries the connector's Retry-After through.
        let limited = MockTransport::new(vec![
            token_ok(3600),
            response_with_retry_after(
                429,
                json!({ "error": { "code": "TooManyRequests", "message": "slow down" } }),
                12.0,
            ),
        ]);
        let result = adapter(&limited)
            .send("19:chat@thread.skype", "hello", None, None)
            .await;
        assert_eq!(result.error_kind.as_deref(), Some("rate_limited"));
        assert!(result.retryable);
        assert_eq!(result.retry_after, Some(12.0));

        // A transport failure is transient and retryable.
        let down = MockTransport::new(vec![
            token_ok(3600),
            Err(TransportError::Request("connection reset".to_string())),
        ]);
        let result = adapter(&down)
            .send("19:chat@thread.skype", "hello", None, None)
            .await;
        assert_eq!(result.error_kind.as_deref(), Some("transient"));
        assert!(result.retryable);
    }

    #[tokio::test]
    async fn a_401_drops_the_cached_token() {
        let mock = MockTransport::new(vec![
            token_ok(3600),
            connector_error(401, "Unauthorized", "token expired"),
            token_ok(3600),
            activity_ok("a-after"),
        ]);
        let client = adapter(&mock);

        let failed = client
            .send("19:chat@thread.skype", "hello", None, None)
            .await;
        assert_eq!(failed.error_kind.as_deref(), Some("forbidden"));
        // The stale token was thrown away, so the next send mints a fresh one.
        let ok = client
            .send("19:chat@thread.skype", "hello again", None, None)
            .await;
        assert!(ok.success, "{:?}", ok.error);
        assert_eq!(mock.token_requests(), 2);
    }

    #[tokio::test]
    async fn a_later_chunk_failure_reports_the_delivered_head() {
        let content = "y".repeat(TEAMS_MAX_MESSAGE_LENGTH * 2);
        let mock = MockTransport::new(vec![
            token_ok(3600),
            activity_ok("a-head"),
            connector_error(403, "Forbidden", "bot lacks ChannelMessage.Send"),
        ]);
        let result = adapter(&mock)
            .send("19:chat@thread.skype", &content, None, None)
            .await;

        assert!(!result.success);
        assert_eq!(result.error_kind.as_deref(), Some("forbidden"));
        // A 403 is not an expired token, so no re-fetch happens on the next call.
        assert_eq!(mock.token_requests(), 1);
        // The chunk that landed is reported so a retry sends only the tail.
        assert_eq!(result.continuation_message_ids, vec!["a-head"]);
        assert!(result.message_id.is_none());
    }

    #[tokio::test]
    async fn blank_content_is_refused_without_a_request() {
        let mock = MockTransport::new(Vec::new());
        let result = adapter(&mock)
            .send("19:chat@thread.skype", "  \n\t ", None, None)
            .await;
        assert!(!result.success);
        assert_eq!(result.error_kind.as_deref(), Some("bad_format"));
        assert_eq!(
            result.error.as_deref(),
            Some("teams cannot send an empty text message")
        );
        assert!(mock.requests().is_empty(), "no request may be made");
    }

    // -- outbound media -----------------------------------------------------------------------

    /// The task's PNG magic: its base64 form (`iVBORw0KGgo=`) is the well-known PNG data-URI prefix,
    /// so the assertion below checks this file's encoder against a published constant.
    const PNG_MAGIC: &[u8] = b"\x89PNG\r\n\x1a\n";

    fn png_media() -> OutboundMedia {
        OutboundMedia::new(MediaKind::Image, MediaSource::Bytes(PNG_MAGIC.to_vec()))
    }

    /// The single `attachments[0]` entry of a serialized activity body.
    fn first_attachment(body: &Value) -> Value {
        body["attachments"].get(0).cloned().unwrap_or(Value::Null)
    }

    #[tokio::test]
    async fn supports_media_reports_attachment_support() {
        let client = adapter(&MockTransport::new(Vec::new()));
        assert!(client.supports_media());
    }

    #[tokio::test]
    async fn send_media_posts_an_attachment_activity_for_held_bytes() {
        let mock = MockTransport::new(vec![token_ok(3600), activity_ok("a-media")]);
        let media = png_media()
            .with_caption("  a cat  ")
            .with_filename("cat.png");
        let result = adapter(&mock)
            .send_media("19:chat@thread.skype", &media, None)
            .await;

        assert!(result.success, "{:?}", result.error);
        assert_eq!(result.message_id.as_deref(), Some("a-media"));
        assert!(result.continuation_message_ids.is_empty());
        assert!(result.error.is_none());
        assert!(result.error_kind.is_none());

        let requests = mock.api_requests();
        assert_eq!(requests.len(), 1);
        assert_eq!(mock.token_requests(), 1);
        assert_eq!(requests[0].method, HttpMethod::Post);
        assert_eq!(requests[0].url, DEFAULT_ACTIVITIES_URL);
        assert_eq!(bearer(&requests[0]).as_deref(), Some("Bearer bf-token"));

        let body = request_body(&requests[0]);
        assert_eq!(body["type"], json!("message"));
        // The caption is the activity `text`, trimmed like any other outbound text.
        assert_eq!(body["text"], json!("a cat"));
        let attachments = body["attachments"].as_array().cloned().unwrap_or_default();
        assert_eq!(attachments.len(), 1, "one attachment per activity");
        assert_eq!(attachments[0]["contentType"], json!("image/png"));
        assert_eq!(
            attachments[0]["contentUrl"],
            json!("data:image/png;base64,iVBORw0KGgo=")
        );
        // No thread anchor was asked for, so none is sent.
        assert!(body.get("replyToId").is_none());
    }

    #[tokio::test]
    async fn media_data_uris_use_the_per_kind_default_mime() {
        // (kind, the default MIME the hermes `send_*` wrapper passes for it)
        let cases = [
            (MediaKind::Image, "image/png"),
            (MediaKind::Video, "video/mp4"),
            (MediaKind::Audio, "audio/mpeg"),
            (MediaKind::Document, "application/octet-stream"),
        ];
        for (kind, mime) in cases {
            let mock = MockTransport::new(vec![token_ok(3600), activity_ok("a-1")]);
            let media = OutboundMedia::new(kind, MediaSource::Bytes(b"foobar".to_vec()));
            let result = adapter(&mock)
                .send_media("19:chat@thread.skype", &media, None)
                .await;
            assert!(result.success, "{kind:?}: {:?}", result.error);

            let attachment = first_attachment(&request_body(&mock.api_requests()[0]));
            assert_eq!(attachment["contentType"], json!(mime), "{kind:?}");
            // base64("foobar") = "Zm9vYmFy", so the whole content URL is checkable here.
            assert_eq!(
                attachment["contentUrl"],
                json!(format!("data:{mime};base64,Zm9vYmFy")),
                "{kind:?}"
            );
        }
    }

    #[tokio::test]
    async fn a_media_mime_override_is_used_and_junk_falls_back() {
        let mock = MockTransport::new(vec![token_ok(3600), activity_ok("a-webp")]);
        let media = OutboundMedia::new(MediaKind::Image, MediaSource::Bytes(b"f".to_vec()))
            .with_mime(" IMAGE/WebP ");
        assert!(
            adapter(&mock)
                .send_media("19:chat@thread.skype", &media, None)
                .await
                .success
        );
        let attachment = first_attachment(&request_body(&mock.api_requests()[0]));
        assert_eq!(attachment["contentType"], json!("image/webp"));
        assert_eq!(
            attachment["contentUrl"],
            json!("data:image/webp;base64,Zg==")
        );

        // A MIME type carrying parameters, or anything else that is not a bare `type/subtype`, is
        // never interpolated into the content URL: the kind default is used instead.
        for bad in [
            "image/png; charset=utf-8",
            "image",
            "   ",
            "text/plain\nX-Injected: 1",
            "a/b/c",
        ] {
            let mock = MockTransport::new(vec![token_ok(3600), activity_ok("a-fallback")]);
            let media = OutboundMedia::new(MediaKind::Audio, MediaSource::Bytes(b"f".to_vec()))
                .with_mime(bad);
            assert!(
                adapter(&mock)
                    .send_media("19:chat@thread.skype", &media, None)
                    .await
                    .success,
                "{bad:?}"
            );
            let attachment = first_attachment(&request_body(&mock.api_requests()[0]));
            assert_eq!(attachment["contentType"], json!("audio/mpeg"), "{bad:?}");
            assert_eq!(
                attachment["contentUrl"],
                json!("data:audio/mpeg;base64,Zg=="),
                "{bad:?}"
            );
        }
    }

    #[tokio::test]
    async fn send_media_attaches_an_http_url_by_reference() {
        for url in [
            "https://example.test/cat.png",
            "http://example.test/cat.png?size=large",
            "HTTPS://example.test/cat.png",
        ] {
            let mock = MockTransport::new(vec![token_ok(3600), activity_ok("a-url")]);
            let media = OutboundMedia::image_url(url).with_caption("look");
            let result = adapter(&mock)
                .send_media("19:chat@thread.skype", &media, None)
                .await;
            assert!(result.success, "{url}: {:?}", result.error);

            let body = request_body(&mock.api_requests()[0]);
            assert_eq!(body["type"], json!("message"));
            assert_eq!(body["text"], json!("look"));
            let attachment = first_attachment(&body);
            // Attached by reference: the URL itself is the content URL, untouched.
            assert_eq!(attachment["contentUrl"], json!(url), "{url}");
            assert_eq!(attachment["contentType"], json!("image/png"), "{url}");
        }
    }

    #[tokio::test]
    async fn send_media_refuses_non_http_urls_without_a_request() {
        for url in [
            "file:///etc/passwd",
            "data:image/png;base64,iVBORw0KGgo=",
            "ftp://example.test/cat.png",
            "//example.test/cat.png",
            "/var/tmp/cat.png",
            "https:/example.test/cat.png",
            "https://",
        ] {
            let mock = MockTransport::new(Vec::new());
            let media = OutboundMedia::image_url(url);
            let result = adapter(&mock)
                .send_media("19:chat@thread.skype", &media, None)
                .await;
            assert!(!result.success, "{url:?} must be refused");
            assert_eq!(result.error_kind.as_deref(), Some("bad_format"), "{url:?}");
            assert!(!result.retryable, "{url:?}");
            assert!(
                result
                    .error
                    .as_deref()
                    .unwrap_or_default()
                    .contains("must be http(s)"),
                "{url:?} lost the reason: {:?}",
                result.error
            );
            assert!(
                mock.requests().is_empty(),
                "{url:?} must not reach the network"
            );
        }
    }

    #[tokio::test]
    async fn send_media_refuses_empty_bytes_and_a_blank_url() {
        let mock = MockTransport::new(Vec::new());
        let empty = OutboundMedia::new(MediaKind::Image, MediaSource::Bytes(Vec::new()));
        let result = adapter(&mock)
            .send_media("19:chat@thread.skype", &empty, None)
            .await;
        assert!(!result.success);
        assert_eq!(result.error_kind.as_deref(), Some("bad_format"));
        assert_eq!(
            result.error.as_deref(),
            Some("teams cannot attach empty media bytes")
        );

        for blank in ["", "   "] {
            let result = adapter(&mock)
                .send_media(
                    "19:chat@thread.skype",
                    &OutboundMedia::image_url(blank),
                    None,
                )
                .await;
            assert!(!result.success, "{blank:?}");
            assert_eq!(
                result.error_kind.as_deref(),
                Some("bad_format"),
                "{blank:?}"
            );
            assert_eq!(
                result.error.as_deref(),
                Some("teams cannot attach an empty media URL"),
                "{blank:?}"
            );
        }
        assert!(mock.requests().is_empty(), "no request may be made");
    }

    #[tokio::test]
    async fn send_media_omits_a_blank_caption_and_a_blank_reply_anchor() {
        for caption in [None, Some("   ")] {
            let mock = MockTransport::new(vec![token_ok(3600), activity_ok("a-quiet")]);
            let mut media =
                OutboundMedia::new(MediaKind::Document, MediaSource::Bytes(PNG_MAGIC.to_vec()));
            media.caption = caption.map(str::to_string);
            let result = adapter(&mock)
                .send_media("19:chat@thread.skype", &media, Some("  "))
                .await;
            assert!(result.success, "{caption:?}: {:?}", result.error);

            let body = request_body(&mock.api_requests()[0]);
            assert_eq!(body["type"], json!("message"));
            // No caption ⇒ no `text` key at all, rather than an empty one Teams would render.
            assert!(body.get("text").is_none(), "{caption:?}");
            // A blank anchor is not a thread.
            assert!(body.get("replyToId").is_none(), "{caption:?}");
            assert_eq!(
                first_attachment(&body)["contentType"],
                json!("application/octet-stream")
            );
        }
    }

    #[tokio::test]
    async fn send_media_threads_the_activity_under_the_reply_anchor() {
        let mock = MockTransport::new(vec![token_ok(3600), activity_ok("a-reply-media")]);
        let media = OutboundMedia::image_url("https://example.test/cat.png");
        let result = adapter(&mock)
            .send_media("19:chat@thread.skype", &media, Some(" a-parent "))
            .await;
        assert!(result.success, "{:?}", result.error);

        let body = request_body(&mock.api_requests()[0]);
        assert_eq!(body["type"], json!("message"));
        assert_eq!(body["replyToId"], json!("a-parent"));
        // No caption was given, so the activity text stays absent.
        assert!(body.get("text").is_none());
        assert_eq!(
            first_attachment(&body)["contentUrl"],
            json!("https://example.test/cat.png")
        );
    }

    #[tokio::test]
    async fn send_media_refuses_attachments_over_the_size_cap() {
        // Teams documents a 28 MB limit for a file attached to a message.
        assert_eq!(TEAMS_MAX_ATTACHMENT_BYTES, 28 * 1024 * 1024);

        let oversized = OutboundMedia::new(
            MediaKind::Video,
            MediaSource::Bytes(vec![0u8; TEAMS_MAX_ATTACHMENT_BYTES + 1]),
        );
        let mock = MockTransport::new(Vec::new());
        let result = adapter(&mock)
            .send_media("19:chat@thread.skype", &oversized, None)
            .await;
        assert!(!result.success);
        assert_eq!(result.error_kind.as_deref(), Some("too_long"));
        assert!(!result.retryable);
        let error = result.error.unwrap_or_default();
        assert!(
            error.contains(&(TEAMS_MAX_ATTACHMENT_BYTES + 1).to_string()),
            "{error}"
        );
        assert!(
            error.contains(&TEAMS_MAX_ATTACHMENT_BYTES.to_string()),
            "{error}"
        );
        assert!(mock.requests().is_empty(), "no request may be made");
    }

    #[tokio::test]
    async fn send_media_targets_the_configured_service_url() {
        let mock = MockTransport::new(vec![token_ok(3600), activity_ok("a-mirror")]);
        let client = adapter(&mock).with_service_url("https://mirror.test/connector/");
        assert!(
            client
                .send_media(
                    "19:chat@thread.skype",
                    &OutboundMedia::image_url("https://example.test/cat.png"),
                    None,
                )
                .await
                .success
        );
        assert_eq!(
            mock.api_requests()[0].url,
            "https://mirror.test/connector/v3/conversations/19%3Achat%40thread.skype/activities"
        );
    }

    #[tokio::test]
    async fn send_media_fails_when_the_token_request_fails() {
        let media = png_media();
        // A refused app identity never reaches the connector.
        let rejected = MockTransport::new(vec![connector_error(
            401,
            "invalid_client_credentials",
            "AADSTS7000215: bad secret",
        )]);
        let result = adapter(&rejected)
            .send_media("19:chat@thread.skype", &media, None)
            .await;
        assert!(!result.success);
        assert_eq!(result.error_kind.as_deref(), Some("forbidden"));
        assert!(!result.retryable);
        assert!(
            rejected.api_requests().is_empty(),
            "no attachment activity may be attempted"
        );

        // An unreachable token endpoint is transient, and equally fatal to the send.
        let down = MockTransport::new(vec![Err(TransportError::Request("dns".to_string()))]);
        let result = adapter(&down)
            .send_media("19:chat@thread.skype", &media, None)
            .await;
        assert!(!result.success);
        assert_eq!(result.error_kind.as_deref(), Some("transient"));
        assert!(result.retryable);
        assert!(down.api_requests().is_empty());
    }

    #[tokio::test]
    async fn media_connector_statuses_map_onto_the_send_taxonomy() {
        let media = png_media();
        // (status, expected kind, retryable)
        for (status, kind, retryable) in [
            (429, "rate_limited", true),
            (401, "forbidden", false),
            (403, "forbidden", false),
            (404, "not_found", false),
            (413, "too_long", false),
            (400, "bad_format", false),
            (500, "transient", true),
            (503, "transient", true),
        ] {
            let mock = MockTransport::new(vec![
                token_ok(3600),
                connector_error(status, "BadArgument", "nope"),
            ]);
            let result = adapter(&mock)
                .send_media("19:chat@thread.skype", &media, None)
                .await;
            assert!(!result.success, "HTTP {status} must fail");
            assert_eq!(result.error_kind.as_deref(), Some(kind), "HTTP {status}");
            assert_eq!(result.retryable, retryable, "HTTP {status}");
            assert!(
                result
                    .error
                    .as_deref()
                    .unwrap_or_default()
                    .contains("BadArgument"),
                "HTTP {status} lost the error detail: {:?}",
                result.error
            );
        }

        // 429 carries the connector's Retry-After through, so the ledger can honour it.
        let limited = MockTransport::new(vec![
            token_ok(3600),
            response_with_retry_after(
                429,
                json!({ "error": { "code": "TooManyRequests", "message": "slow down" } }),
                7.0,
            ),
        ]);
        let result = adapter(&limited)
            .send_media("19:chat@thread.skype", &media, None)
            .await;
        assert_eq!(result.error_kind.as_deref(), Some("rate_limited"));
        assert!(result.retryable);
        assert_eq!(result.retry_after, Some(7.0));

        // A transport error produced no response at all: transient, retryable, named as such.
        let down = MockTransport::new(vec![
            token_ok(3600),
            Err(TransportError::Request("connection reset".to_string())),
        ]);
        let result = adapter(&down)
            .send_media("19:chat@thread.skype", &media, None)
            .await;
        assert!(!result.success);
        assert_eq!(result.error_kind.as_deref(), Some("transient"));
        assert!(result.retryable);
        assert!(
            result
                .error
                .as_deref()
                .unwrap_or_default()
                .contains("transport error"),
            "{:?}",
            result.error
        );

        // A 401 on the attachment drops the cached token, exactly like a text send.
        let expired = MockTransport::new(vec![
            token_ok(3600),
            connector_error(401, "Unauthorized", "token expired"),
            token_ok(3600),
            activity_ok("a-after-401"),
        ]);
        let client = adapter(&expired);
        assert!(
            !client
                .send_media("19:chat@thread.skype", &media, None)
                .await
                .success
        );
        assert!(
            client
                .send_media("19:chat@thread.skype", &media, None)
                .await
                .success
        );
        assert_eq!(expired.token_requests(), 2);
    }

    #[tokio::test]
    async fn a_media_response_without_an_activity_id_is_a_failure() {
        let mock = MockTransport::new(vec![
            token_ok(3600),
            response(200, json!({ "id": "   " })),
            response(200, json!({})),
        ]);
        let client = adapter(&mock);
        let media = png_media();
        for _ in 0..2 {
            let result = client
                .send_media("19:chat@thread.skype", &media, None)
                .await;
            assert!(
                !result.success,
                "a 200 without an activity id is not a delivered attachment"
            );
            assert!(result.message_id.is_none());
            assert_eq!(result.error_kind.as_deref(), Some("unknown"));
        }
    }

    // -- outbound adaptive cards --------------------------------------------------------------

    /// A card shaped like the SDK model the reference sends: typed, versioned, with a body and an
    /// action (`AdaptiveCard().with_version("1.4").with_body(body).with_actions(actions)`,
    /// adapter.py:709).
    fn adaptive_card() -> Value {
        json!({
            "type": "AdaptiveCard",
            "version": "1.4",
            "$schema": "http://adaptivecards.io/schemas/adaptive-card.json",
            "body": [{ "type": "TextBlock", "text": "Deploy to prod?", "wrap": true }],
            "actions": [{
                "type": "Action.Execute",
                "title": "Approve",
                "verb": "hermes_approve",
                "data": { "hermes_action": "approve_once", "session_key": "s-1" },
            }],
        })
    }

    #[tokio::test]
    async fn supports_cards_reports_adaptive_card_support() {
        let client = adapter(&MockTransport::new(Vec::new()));
        assert!(client.supports_cards());
    }

    #[tokio::test]
    async fn send_card_posts_one_adaptive_attachment_activity() {
        let mock = MockTransport::new(vec![token_ok(3600), activity_ok("a-card")]);
        let card = adaptive_card();
        let result = adapter(&mock)
            .send_card("19:chat@thread.skype", &card)
            .await;

        assert!(result.success, "{:?}", result.error);
        assert_eq!(result.message_id.as_deref(), Some("a-card"));
        assert!(result.error.is_none());
        assert!(result.error_kind.is_none());
        assert!(result.continuation_message_ids.is_empty());

        let requests = mock.api_requests();
        assert_eq!(requests.len(), 1);
        assert_eq!(mock.token_requests(), 1);
        assert_eq!(requests[0].method, HttpMethod::Post);
        assert_eq!(requests[0].url, DEFAULT_ACTIVITIES_URL);
        assert_eq!(bearer(&requests[0]).as_deref(), Some("Bearer bf-token"));

        let body = request_body(&requests[0]);
        assert_eq!(body["type"], json!("message"));
        // The card is the activity's whole content: no text, no thread anchor.
        assert!(body.get("text").is_none());
        assert!(body.get("replyToId").is_none());
        let attachments = body["attachments"].as_array().cloned().unwrap_or_default();
        assert_eq!(attachments.len(), 1, "one adaptive attachment per activity");
        assert_eq!(
            attachments[0]["contentType"],
            json!(TEAMS_ADAPTIVE_CARD_CONTENT_TYPE)
        );
        assert_eq!(
            attachments[0]["contentType"],
            json!("application/vnd.microsoft.card.adaptive")
        );
        // Passed through untouched: no key is added, dropped or retyped.
        assert_eq!(attachments[0]["content"], card);
    }

    #[tokio::test]
    async fn a_minimal_typed_card_is_accepted_verbatim() {
        let mock = MockTransport::new(vec![token_ok(3600), activity_ok("a-min")]);
        // `version`/`body`/`$schema` are the caller's card, not this adapter's: nothing is invented.
        let card = json!({ "type": "AdaptiveCard", "body": [] });
        let result = adapter(&mock)
            .send_card("19:chat@thread.skype", &card)
            .await;
        assert!(result.success, "{:?}", result.error);
        assert_eq!(
            first_attachment(&request_body(&mock.api_requests()[0]))["content"],
            card
        );
    }

    #[tokio::test]
    async fn send_card_targets_the_configured_service_url() {
        let mock = MockTransport::new(vec![token_ok(3600), activity_ok("a-mirror-card")]);
        let client = adapter(&mock).with_service_url("https://mirror.test/connector/");
        assert!(
            client
                .send_card("19:chat@thread.skype", &adaptive_card())
                .await
                .success
        );
        // No metadata on the card path either, so the configured base is the target.
        assert_eq!(
            mock.api_requests()[0].url,
            "https://mirror.test/connector/v3/conversations/19%3Achat%40thread.skype/activities"
        );
    }

    #[tokio::test]
    async fn send_card_refuses_an_untyped_or_non_object_card() {
        // (card, the phrase the refusal must carry)
        let cases = [
            (json!([]), "an array"),
            (json!(["type"]), "an array"),
            (json!("{\"type\":\"AdaptiveCard\"}"), "a string"),
            (json!(42), "a number"),
            (Value::Null, "null"),
            (json!({}), "empty Adaptive Card object"),
            // A card without a `type` is never retyped here: the reference always sends a typed one.
            (json!({ "body": [] }), "no \"type\""),
            (json!({ "version": "1.4" }), "no \"type\""),
            // A different card type belongs under a different contentType.
            (
                json!({ "type": "MessageCard", "text": "legacy" }),
                "MessageCard",
            ),
            (json!({ "type": "   " }), "no \"type\""),
        ];
        for (card, needle) in cases {
            let mock = MockTransport::new(Vec::new());
            let result = adapter(&mock)
                .send_card("19:chat@thread.skype", &card)
                .await;
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
    async fn a_card_response_without_an_activity_id_is_a_failure() {
        let mock = MockTransport::new(vec![
            token_ok(3600),
            response(200, json!({})),
            response(200, json!({ "id": "   " })),
        ]);
        let client = adapter(&mock);
        for attempt in 0..2 {
            let result = client
                .send_card("19:chat@thread.skype", &adaptive_card())
                .await;
            assert!(
                !result.success,
                "attempt {attempt}: a 200 without an activity id is not a delivered card"
            );
            assert!(result.message_id.is_none());
            assert_eq!(result.error_kind.as_deref(), Some("unknown"));
            let error = result.error.unwrap_or_default();
            assert!(
                error.contains("card activity response carried no activity id"),
                "{error}"
            );
        }
    }

    #[tokio::test]
    async fn card_connector_statuses_map_onto_the_send_taxonomy() {
        for (status, kind, retryable) in [
            (429, "rate_limited", true),
            (401, "forbidden", false),
            (403, "forbidden", false),
            (404, "not_found", false),
            (413, "too_long", false),
            (400, "bad_format", false),
            (500, "transient", true),
            (503, "transient", true),
        ] {
            let mock = MockTransport::new(vec![
                token_ok(3600),
                connector_error(status, "BadArgument", "nope"),
            ]);
            let result = adapter(&mock)
                .send_card("19:chat@thread.skype", &adaptive_card())
                .await;
            assert!(!result.success, "HTTP {status} must fail");
            assert_eq!(result.error_kind.as_deref(), Some(kind), "HTTP {status}");
            assert_eq!(result.retryable, retryable, "HTTP {status}");
            assert!(result.message_id.is_none(), "HTTP {status}");
            let error = result.error.unwrap_or_default();
            assert!(
                error.contains("BadArgument"),
                "HTTP {status} lost the detail: {error}"
            );
        }

        // 429 carries the connector's Retry-After through.
        let limited = MockTransport::new(vec![
            token_ok(3600),
            response_with_retry_after(
                429,
                json!({ "error": { "code": "TooManyRequests", "message": "slow down" } }),
                9.0,
            ),
        ]);
        let result = adapter(&limited)
            .send_card("19:chat@thread.skype", &adaptive_card())
            .await;
        assert_eq!(result.error_kind.as_deref(), Some("rate_limited"));
        assert!(result.retryable);
        assert_eq!(result.retry_after, Some(9.0));

        // A transport error produced no response at all: transient and retryable.
        let down = MockTransport::new(vec![
            token_ok(3600),
            Err(TransportError::Request("connection reset".to_string())),
        ]);
        let result = adapter(&down)
            .send_card("19:chat@thread.skype", &adaptive_card())
            .await;
        assert_eq!(result.error_kind.as_deref(), Some("transient"));
        assert!(result.retryable);
        assert!(
            result
                .error
                .as_deref()
                .unwrap_or_default()
                .contains("card send transport error"),
            "{:?}",
            result.error
        );

        // A 401 on the card drops the cached token, exactly like a text or media send.
        let expired = MockTransport::new(vec![
            token_ok(3600),
            connector_error(401, "Unauthorized", "token expired"),
            token_ok(3600),
            activity_ok("a-after-401"),
        ]);
        let client = adapter(&expired);
        assert!(
            !client
                .send_card("19:chat@thread.skype", &adaptive_card())
                .await
                .success
        );
        assert!(
            client
                .send_card("19:chat@thread.skype", &adaptive_card())
                .await
                .success
        );
        assert_eq!(expired.token_requests(), 2);
    }

    #[tokio::test]
    async fn send_card_fails_when_the_token_request_fails() {
        let rejected = MockTransport::new(vec![connector_error(
            401,
            "invalid_client_credentials",
            "AADSTS7000215: bad secret",
        )]);
        let result = adapter(&rejected)
            .send_card("19:chat@thread.skype", &adaptive_card())
            .await;
        assert!(!result.success);
        assert_eq!(result.error_kind.as_deref(), Some("forbidden"));
        assert!(!result.retryable);
        assert!(
            rejected.api_requests().is_empty(),
            "no card activity may be attempted"
        );
    }

    #[tokio::test]
    async fn an_adaptive_card_helper_names_the_json_that_arrived() {
        assert_eq!(json_type_name(&json!([])), "an array");
        assert_eq!(json_type_name(&json!({})), "an object");
        assert_eq!(json_type_name(&json!(true)), "a boolean");
        assert_eq!(json_type_name(&Value::Null), "null");

        // The accepted shape is the card object itself, unchanged.
        let card = adaptive_card();
        assert_eq!(adaptive_card_content(&card).ok(), Some(card.clone()));
        let activity = card_activity(&card);
        assert_eq!(activity["type"], json!("message"));
        assert_eq!(
            activity["attachments"][0]["contentType"],
            json!(TEAMS_ADAPTIVE_CARD_CONTENT_TYPE)
        );
        assert_eq!(activity["attachments"][0]["content"], card);
        assert!(activity.get("text").is_none());
    }

    #[test]
    fn base64_matches_the_rfc_4648_vectors() {
        assert_eq!(base64_encode(b""), "");
        assert_eq!(base64_encode(b"f"), "Zg==");
        assert_eq!(base64_encode(b"fo"), "Zm8=");
        assert_eq!(base64_encode(b"foo"), "Zm9v");
        assert_eq!(base64_encode(b"foob"), "Zm9vYg==");
        assert_eq!(base64_encode(b"fooba"), "Zm9vYmE=");
        assert_eq!(base64_encode(b"foobar"), "Zm9vYmFy");
        assert_eq!(base64_encode(&[0xff, 0x00, 0x7f]), "/wB/");
        // The PNG signature's published base64 form, which is also the fixture above.
        assert_eq!(base64_encode(PNG_MAGIC), "iVBORw0KGgo=");
        assert_eq!(data_uri("image/png", b"foo"), "data:image/png;base64,Zm9v");
    }

    #[test]
    fn media_helpers_refuse_junk() {
        assert!(is_media_type("image/png"));
        assert!(is_media_type(
            "application/vnd.openxmlformats-officedocument.wordprocessingml.document"
        ));
        assert!(!is_media_type(""));
        assert!(!is_media_type("image"));
        assert!(!is_media_type("image/"));
        assert!(!is_media_type("/png"));
        assert!(!is_media_type("a/b/c"));
        assert!(!is_media_type("image/png; charset=utf-8"));
        assert!(!is_media_type("image/p ng"));
        assert!(!is_media_type("image/png\r\nX: y"));

        assert!(is_http_url("https://x.test/a"));
        assert!(is_http_url("http://x.test"));
        assert!(is_http_url("HtTpS://x.test/a?b=1#c"));
        assert!(!is_http_url(""));
        assert!(!is_http_url("x.test/a"));
        assert!(!is_http_url("file:///etc/passwd"));
        assert!(!is_http_url("data:image/png;base64,AA=="));
        assert!(!is_http_url("ftp://x.test/a"));
        assert!(!is_http_url("https://"));
        assert!(!is_http_url("https:///path"));

        assert_eq!(default_media_mime(MediaKind::Image), "image/png");
        assert_eq!(default_media_mime(MediaKind::Video), "video/mp4");
        assert_eq!(default_media_mime(MediaKind::Audio), "audio/mpeg");
        assert_eq!(
            default_media_mime(MediaKind::Document),
            "application/octet-stream"
        );

        // The activity body only carries the keys that were asked for.
        let bare = media_activity("image/png", "https://x.test/a.png", None, None);
        assert_eq!(bare["type"], json!("message"));
        assert_eq!(bare["attachments"][0]["contentType"], json!("image/png"));
        assert_eq!(
            bare["attachments"][0]["contentUrl"],
            json!("https://x.test/a.png")
        );
        assert!(bare.get("text").is_none());
        assert!(bare.get("replyToId").is_none());
        let full = media_activity(
            "image/png",
            "data:image/png;base64,Zm9v",
            Some(" hi "),
            Some(" a-1 "),
        );
        assert_eq!(full["text"], json!("hi"));
        assert_eq!(full["replyToId"], json!("a-1"));
    }

    #[tokio::test]
    async fn send_typing_posts_a_typing_activity_and_swallows_failures() {
        let mock = MockTransport::new(vec![token_ok(3600), activity_ok("t-1")]);
        let client = adapter(&mock);
        client.send_typing("19:chat@thread.skype", None).await;

        let requests = mock.api_requests();
        assert_eq!(requests.len(), 1);
        assert_eq!(requests[0].method, HttpMethod::Post);
        assert_eq!(requests[0].url, DEFAULT_ACTIVITIES_URL);
        assert_eq!(request_body(&requests[0])["type"], json!("typing"));
        assert_eq!(bearer(&requests[0]).as_deref(), Some("Bearer bf-token"));
        // The event's service_url rides on the typing activity too.
        client
            .send_typing(
                "19:chat@thread.skype",
                Some(json!({ "service_url": "https://mirror.test/connector" })),
            )
            .await;

        // A transport error and a rejected typing activity are both swallowed.
        let failing = MockTransport::new(vec![
            token_ok(3600),
            Err(TransportError::Request("timeout".to_string())),
            connector_error(500, "Internal", "boom"),
        ]);
        adapter(&failing)
            .send_typing("19:chat@thread.skype", None)
            .await;
        adapter(&failing)
            .send_typing("19:chat@thread.skype", None)
            .await;
        // A missing token makes typing a no-op instead of a panic.
        let no_token = MockTransport::new(vec![Err(TransportError::Request("dns".to_string()))]);
        adapter(&no_token)
            .send_typing("19:chat@thread.skype", None)
            .await;
        assert!(no_token.api_requests().is_empty());
    }

    #[tokio::test]
    async fn edit_updates_and_delete_removes_an_activity() {
        let mock = MockTransport::new(vec![token_ok(3600), activity_ok("a-9")]);
        let client = adapter(&mock);
        let result = client
            .edit_message("19:chat@thread.skype", "a-9", "new text", false)
            .await;

        assert!(result.success);
        assert_eq!(result.message_id.as_deref(), Some("a-9"));
        let requests = mock.api_requests();
        assert_eq!(requests.len(), 1);
        assert_eq!(requests[0].method, HttpMethod::Put);
        assert_eq!(
            requests[0].url,
            "https://smba.trafficmanager.net/teams/v3/conversations/19%3Achat%40thread.skype/activities/a-9"
        );
        let body = request_body(&requests[0]);
        assert_eq!(body["type"], json!("message"));
        assert_eq!(body["text"], json!("new text"));
        assert_eq!(bearer(&requests[0]).as_deref(), Some("Bearer bf-token"));

        // Delete issues a DELETE for the same activity path and never fails its caller.
        let mock = MockTransport::new(vec![token_ok(3600), response(200, json!({}))]);
        let client = adapter(&mock);
        client.delete_message("19:chat@thread.skype", "a-9").await;
        let requests = mock.api_requests();
        assert_eq!(requests.len(), 1);
        assert_eq!(requests[0].method, HttpMethod::Delete);
        assert_eq!(
            requests[0].url,
            "https://smba.trafficmanager.net/teams/v3/conversations/19%3Achat%40thread.skype/activities/a-9"
        );
        assert!(requests[0].body.is_none());
        assert_eq!(bearer(&requests[0]).as_deref(), Some("Bearer bf-token"));
        // A failing delete is swallowed as well.
        let failing = MockTransport::new(vec![
            token_ok(3600),
            connector_error(404, "NotFound", "activity is gone"),
        ]);
        adapter(&failing)
            .delete_message("19:chat@thread.skype", "a-gone")
            .await;
    }

    #[tokio::test]
    async fn an_edit_failure_uses_the_honest_taxonomy() {
        let missing = MockTransport::new(vec![
            token_ok(3600),
            connector_error(404, "NotFound", "activity is gone"),
        ]);
        let result = adapter(&missing)
            .edit_message("19:chat@thread.skype", "a-gone", "new text", false)
            .await;
        assert!(!result.success);
        assert_eq!(result.error_kind.as_deref(), Some("not_found"));
        assert!(!result.retryable);

        // A blank update is refused before any request.
        let blank = MockTransport::new(Vec::new());
        let result = adapter(&blank)
            .edit_message("19:chat@thread.skype", "a-1", "   ", false)
            .await;
        assert_eq!(result.error_kind.as_deref(), Some("bad_format"));
        assert!(blank.requests().is_empty());

        // A 401 on the update drops the cached token so the retry mints a new one.
        let expired = MockTransport::new(vec![
            token_ok(3600),
            connector_error(401, "Unauthorized", "token expired"),
            token_ok(3600),
            activity_ok("a-1"),
        ]);
        let client = adapter(&expired);
        assert!(
            !client
                .edit_message("19:chat@thread.skype", "a-1", "one", false)
                .await
                .success
        );
        assert!(
            client
                .edit_message("19:chat@thread.skype", "a-1", "two", false)
                .await
                .success
        );
        assert_eq!(expired.token_requests(), 2);
    }

    #[tokio::test]
    async fn get_chat_info_answers_locally() {
        let mock = MockTransport::new(Vec::new());
        let info = adapter(&mock).get_chat_info("19:chat@thread.skype").await;
        assert_eq!(info.name, "19:chat@thread.skype");
        assert_eq!(info.chat_type, "group");
        assert!(
            mock.requests().is_empty(),
            "there is no cheap conversation read to call"
        );
    }

    #[test]
    fn webhook_parses_a_personal_message_activity() {
        let client = TeamsAdapter::new(MockTransport::new(Vec::new()), "t", "c", "s");
        let body = json!({
            "type": "message",
            "id": "1:activity-1",
            "text": "hello there",
            "from": { "id": "28:bot-user", "name": "Ann", "aadObjectId": "aad-ann" },
            "conversation": {
                "id": "a:personal-conv",
                "conversationType": "personal",
                "tenantId": "tenant-1",
                "name": "Ann",
            },
            "channelId": "msteams",
            "serviceUrl": "https://smba.trafficmanager.net/teams/",
            "replyToId": "1:parent",
            "timestamp": "2023-11-14T22:13:20.123Z",
        })
        .to_string();
        let events = client.parse_webhook(&body).unwrap_or_default();

        assert_eq!(events.len(), 1);
        let event = &events[0];
        assert_eq!(event.text, "hello there");
        assert_eq!(event.message_id.as_deref(), Some("1:activity-1"));
        assert_eq!(event.reply_to_message_id.as_deref(), Some("1:parent"));
        assert_eq!(event.platform_update_id, Some(1_700_000_000_123));
        assert_eq!(event.source.platform.as_str(), "teams");
        assert_eq!(event.source.chat_id, "a:personal-conv");
        assert_eq!(event.source.chat_type, "dm");
        assert_eq!(event.source.chat_name.as_deref(), Some("Ann"));
        assert_eq!(event.source.user_id.as_deref(), Some("aad-ann"));
        assert_eq!(event.source.user_name.as_deref(), Some("Ann"));
        assert_eq!(event.source.message_id.as_deref(), Some("1:activity-1"));
        assert_eq!(event.source.scope_id.as_deref(), Some("tenant-1"));
        assert_eq!(event.source.guild_id.as_deref(), Some("tenant-1"));
        assert_eq!(event.user_id.as_deref(), Some("aad-ann"));
        assert_eq!(event.user_name.as_deref(), Some("Ann"));
        assert!(!event.source.is_bot);
        assert!(event.raw_message.is_some());
        // The connector base rides on the event so a reply is routable without state.
        assert_eq!(
            event.metadata.get("service_url").and_then(Value::as_str),
            Some("https://smba.trafficmanager.net/teams/")
        );
        // The event round-trips through the gateway's own source routing.
        assert_eq!(event.source.description(), "DM with Ann");
    }

    #[test]
    fn webhook_maps_channel_and_group_chat_activities_to_a_group() {
        let client = TeamsAdapter::new(MockTransport::new(Vec::new()), "t", "c", "s");
        for (conversation_type, expected) in [
            ("channel", "group"),
            ("groupChat", "group"),
            ("personal", "dm"),
            ("somethingNew", "dm"),
        ] {
            let body = json!({
                "type": "message",
                "id": "1:a",
                "text": "hi",
                "from": { "id": "user-1" },
                "conversation": { "id": "19:conv@thread.skype", "conversationType": conversation_type },
                "timestamp": "2024-05-01T12:00:00.000Z",
            })
            .to_string();
            let events = client.parse_webhook(&body).unwrap_or_default();
            assert_eq!(events.len(), 1, "{conversation_type}");
            assert_eq!(events[0].source.chat_type, expected, "{conversation_type}");
            assert_eq!(events[0].source.user_id.as_deref(), Some("user-1"));
            assert_eq!(events[0].platform_update_id, Some(1_714_564_800_000));
            // No serviceUrl ⇒ no routing metadata is fabricated.
            assert!(events[0].metadata.is_empty());
        }
    }

    #[test]
    fn webhook_ignores_unrelated_activities_and_bad_payloads() {
        let client = TeamsAdapter::new(MockTransport::new(Vec::new()), "t", "c", "s");
        for activity in [
            // Not a message activity.
            json!({
                "type": "conversationUpdate",
                "conversation": { "id": "a:conv" },
                "text": "someone joined",
            }),
            json!({ "type": "typing", "conversation": { "id": "a:conv" } }),
            json!({ "type": "invoke", "conversation": { "id": "a:conv" }, "text": "x" }),
            json!({ "type": "messageReaction", "conversation": { "id": "a:conv" } }),
            // A message with no text, and one with no conversation.
            json!({ "type": "message", "id": "1:a", "conversation": { "id": "a:conv" } }),
            json!({ "type": "message", "id": "1:a", "text": "   ", "conversation": { "id": "a:conv" } }),
            json!({ "type": "message", "id": "1:a", "text": "hi" }),
            json!({ "type": "message", "id": "1:a", "text": "hi", "conversation": {} }),
        ] {
            assert!(
                client
                    .parse_webhook(&activity.to_string())
                    .unwrap_or_default()
                    .is_empty(),
                "{activity} must not produce an event"
            );
        }

        // Invalid JSON is an error, not a silent skip.
        let error = client
            .parse_webhook("not json")
            .expect_err("invalid json must fail");
        assert!(error.contains("invalid json"), "{error}");
    }

    #[test]
    fn timestamps_parse_with_fractions_and_offsets() {
        assert_eq!(parse_timestamp_millis("1970-01-01T00:00:00Z"), Some(0));
        assert_eq!(
            parse_timestamp_millis("2023-11-14T22:13:20.123Z"),
            Some(1_700_000_000_123)
        );
        // One fractional digit is 100 ms, three are milliseconds, extras are truncated.
        assert_eq!(
            parse_timestamp_millis("2023-11-14T22:13:20.1Z"),
            Some(1_700_000_000_100)
        );
        assert_eq!(
            parse_timestamp_millis("2023-11-14T22:13:20.1239Z"),
            Some(1_700_000_000_123)
        );
        // A +02:00 offset denotes the same instant, written two hours later.
        assert_eq!(
            parse_timestamp_millis("2024-05-01T14:00:00+02:00"),
            Some(1_714_564_800_000)
        );
        assert_eq!(
            parse_timestamp_millis("2024-05-01T14:00:00+0200"),
            Some(1_714_564_800_000)
        );
        // A -02:00 offset means the local clock is two hours behind UTC, so 22:00 local is
        // 2024-05-01T00:00:00Z — the same instant the +02:00 cases name from the other side.
        assert_eq!(
            parse_timestamp_millis("2024-04-30T22:00:00-02:00"),
            Some(1_714_521_600_000)
        );
        assert_eq!(parse_timestamp_millis("2024-05-01T12:00:00"), None);
        assert_eq!(parse_timestamp_millis("not a timestamp"), None);
        assert_eq!(parse_timestamp_millis("2024-13-01T00:00:00Z"), None);
        assert_eq!(parse_timestamp_millis("2024-05-01T25:00:00Z"), None);
        assert_eq!(parse_timestamp_millis(""), None);
    }

    #[test]
    fn url_and_error_helpers() {
        assert_eq!(encode_path_segment("abc-123_x.y~z"), "abc-123_x.y~z");
        assert_eq!(
            encode_path_segment("19:a@thread.skype;messageid=1"),
            "19%3Aa%40thread.skype%3Bmessageid%3D1"
        );
        assert_eq!(
            activities_url("https://x/", "a:b"),
            "https://x/v3/conversations/a%3Ab/activities"
        );
        assert_eq!(
            activity_url("https://x", "a:b", "1:c"),
            "https://x/v3/conversations/a%3Ab/activities/1%3Ac"
        );

        assert_eq!(
            render_error(
                400,
                &json!({ "error": { "code": "BadArgument", "message": "bad text" } })
            ),
            "teams error BadArgument (HTTP 400): bad text"
        );
        assert_eq!(
            render_error(400, &json!({ "message": "invalid_client" })),
            "teams HTTP 400: invalid_client"
        );
        assert_eq!(render_error(404, &json!({})), "teams HTTP 404");
        assert_eq!(value_to_i64(&json!(3600)), Some(3600));
        assert_eq!(value_to_i64(&json!("3600")), Some(3600));
        assert_eq!(value_to_i64(&json!("soon")), None);
        assert_eq!(value_to_i64(&json!(null)), None);
    }

    #[tokio::test]
    async fn capability_surface_matches_the_connector() {
        let client = adapter(&MockTransport::new(Vec::new()));
        assert!(client.splits_long_messages());
        assert_eq!(client.max_message_length(), 28000);
        assert_eq!(client.message_length("héllo"), 5);
        assert_eq!(TEAMS_MAX_MESSAGE_LENGTH, 28000);
        // Editing is implemented, so the adapter is not lying about `delete_message` either.
        assert_eq!(client.typed_command_prefix(), "/");
    }

    // -- inbound activity JWT verification ----------------------------------------------------

    /// A transport serving the fixture JWKS, plus an adapter whose `client_id` is the audience the
    /// tests sign for.
    fn verifying_adapter(client_id: &str) -> (TeamsAdapter, Arc<MockTransport>) {
        let transport = MockTransport::new(vec![Ok(HttpResponse {
            status: 200,
            body: rsa_jwks().to_string(),
            retry_after: None,
        })]);
        let client = TeamsAdapter::new(
            Arc::clone(&transport) as SharedTransport,
            "tenant-1",
            client_id,
            "s3cret",
        );
        (client, transport)
    }

    /// Claims shaped like the ones the Bot Framework puts in an activity token.
    fn bot_framework_claims(app_id: &str) -> Value {
        json!({
            "iss": BOT_FRAMEWORK_ISSUER,
            "aud": app_id,
            "serviceurl": "https://smba.trafficmanager.net/teams/",
            "nbf": 1_600_000_000u64,
            // Well past any test run's clock: 2096.
            "exp": 4_000_000_000u64,
        })
    }

    #[tokio::test]
    async fn verify_request_accepts_a_bot_framework_token() {
        let (client, transport) = verifying_adapter("client-1");
        let token = sign_rs256(RSA_KID, &bot_framework_claims("client-1"));

        assert!(client.verify_request(&format!("Bearer {token}")).await);
        let requests = transport.requests();
        assert_eq!(requests.len(), 1);
        assert_eq!(requests[0].method, HttpMethod::Get);
        assert_eq!(requests[0].url, BOT_FRAMEWORK_JWKS_URL);

        // The scheme is case-insensitive and the JWKS is reused from the cache.
        assert!(client.verify_request(&format!("bearer {token}")).await);
        assert_eq!(transport.requests().len(), 1);
    }

    #[tokio::test]
    async fn verify_request_rejects_missing_or_garbage_authorization() {
        let (client, transport) = verifying_adapter("client-1");

        for header in [
            "",
            "Bearer",
            "Bearer ",
            "nonsense",
            "Basic dXNlcjpwYXNz",
            "Bearer not.a.token",
        ] {
            assert!(
                !client.verify_request(header).await,
                "{header:?} must not verify"
            );
        }
        // Nothing decodable, so the JWKS was never fetched.
        assert!(transport.requests().is_empty());

        // Another bot's app id, another issuer, an expired token, and a token signed with a key the
        // Bot Framework never published.
        let mut other_issuer = bot_framework_claims("client-1");
        other_issuer["iss"] = json!("https://api.botframework.com.evil.test");
        let mut expired = bot_framework_claims("client-1");
        expired["exp"] = json!(1_000_000_000u64);
        let signed = sign_rs256(RSA_KID, &bot_framework_claims("client-1"));
        let forged = {
            // Same header and signature, different claims: the signature no longer covers them.
            let mut parts = signed.split('.');
            let header = parts.next().unwrap_or_default();
            let signature = parts.nth(1).unwrap_or_default();
            let body = crate::jwt::base64url_encode(
                json!({"iss": BOT_FRAMEWORK_ISSUER, "aud": "client-1", "exp": 4_000_000_000u64})
                    .to_string()
                    .as_bytes(),
            );
            format!("{header}.{body}.{signature}")
        };

        for token in [
            sign_rs256(RSA_KID, &bot_framework_claims("someone-elses-bot")),
            sign_rs256(RSA_KID, &other_issuer),
            sign_rs256(RSA_KID, &expired),
            sign_rs256(
                "a-kid-botframework-never-published",
                &bot_framework_claims("client-1"),
            ),
            forged,
        ] {
            assert!(
                !client.verify_request(&format!("Bearer {token}")).await,
                "token must not verify: {token}"
            );
        }
        assert_eq!(transport.requests().len(), 1);
    }

    #[tokio::test]
    async fn verify_request_without_a_client_id_fails_closed() {
        let (client, transport) = verifying_adapter("   ");
        let token = sign_rs256(RSA_KID, &bot_framework_claims("client-1"));
        assert!(!client.verify_request(&format!("Bearer {token}")).await);
        assert!(
            transport.requests().is_empty(),
            "an adapter with no audience must not even fetch keys"
        );
    }

    #[tokio::test]
    async fn verify_request_treats_a_jwks_outage_as_a_refusal() {
        let transport = MockTransport::new(vec![Err(TransportError::Request("dns".to_string()))]);
        let client = TeamsAdapter::new(
            transport.clone() as SharedTransport,
            "tenant-1",
            "client-1",
            "s3cret",
        );
        let token = sign_rs256(RSA_KID, &bot_framework_claims("client-1"));
        assert!(!client.verify_request(&format!("Bearer {token}")).await);
        assert_eq!(transport.requests().len(), 1);
        // A transport failure is not cached, so the next attempt asks again.
        assert!(!client.verify_request(&format!("Bearer {token}")).await);
        assert_eq!(transport.requests().len(), 2);
    }
}
