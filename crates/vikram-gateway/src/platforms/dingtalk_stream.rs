//! DingTalk **Stream Mode** — the long-connection (WebSocket) transport, ported from
//! `research/hermes-platforms/dingtalk/stream-lib/` (`stream.py`, `frames.py`, `chatbot.py`,
//! `handlers.py`, `credential.py`) plus the reply path of the hermes adapter
//! (`research/hermes-platforms/dingtalk/adapter.py`).
//!
//! Unlike [`super::dingtalk`] — a stateless custom-robot webhook — Stream Mode is a pull
//! transport: the process POSTs an *open connection* request, receives a short-lived
//! `wss://{endpoint}?ticket=…` URL, then answers server frames until the socket closes.
//! [`DingTalkStream`] is the host-facing half ([`DingTalkStream::connect`],
//! [`DingTalkStream::next_event`], [`DingTalkStream::reply`]); [`DingTalkStreamClient`] is the
//! protocol half (open-connection request, frame routing, ACKs, reconnect policy).
//!
//! # Ported behaviour (with the reference line that defines it)
//!
//! - Open-connection request: `POST https://api.dingtalk.com/v1.0/gateway/connections/open`
//!   (`stream.py:28`, origin from `utils.py:8-10`) with `Content-Type: application/json`,
//!   `Accept: application/json` and the `DingTalkStream/1.0 SDK/… (+https://github.com/
//!   open-dingtalk/dingtalk-stream-sdk-python)` user agent (`stream.py:150-156`). Body:
//!   `clientId`, `clientSecret`, `subscriptions`, `ua`, `localIp` (`stream.py:157-168`).
//!   `subscriptions` is `{"type":"EVENT","topic":"*"}` when events are wanted, followed by one
//!   `{"type":"CALLBACK","topic":…}` per registered topic (`stream.py:157-161`). `ua` is
//!   `dingtalk-sdk-python/v{VERSION}-union` (`stream.py:166`). `localIp` is the local address of
//!   a UDP socket "connected" to `8.8.8.8:80` — no packet is sent — and `""` when that fails
//!   (`stream.py:183-195`).
//! - URL building: `f'{endpoint}?ticket={quote_plus(ticket)}'` (`stream.py:73`). [`quote_plus`]
//!   is `urllib.parse.quote_plus` with the default empty `safe` set: `A-Za-z0-9_.-~` survive,
//!   space becomes `+`, everything else is `%XX` over UTF-8 bytes.
//! - Frame model: `type` ∈ `SYSTEM`/`EVENT`/`CALLBACK` (`frames.py:89,126,163`); camelCase
//!   headers with `time` as an epoch-millisecond integer and unlisted headers kept as
//!   extensions (`frames.py:57-85`); `data` is a JSON **string** that is parsed into the frame
//!   (`frames.py:107-123`). An ACK is `{"code", "headers", "message", "data"}` where `data` is
//!   re-serialized to a string and `headers` goes through `Headers.to_dict` — `extensions` plus
//!   `appId`/`connectionId`/`contentType`/`messageId`/`topic`/`time`, with `time` stringified
//!   (`frames.py:41-55,233-239`). Each handler then puts a **fresh** header block on its ACK —
//!   only the frame's `messageId` plus `contentType: application/json` (`handlers.py:30-31`) — so
//!   no other inbound header is echoed back.
//! - ACK bodies: `SYSTEM` frames are answered `200`/`"OK"` with the frame data echoed
//!   (`handlers.py:58-77`); `EVENT` frames get the library's default handler answer
//!   `404`/`"not implement"` (`handlers.py:44-55`, `frames.py:224`); `CALLBACK` frames get
//!   `code`, `messageId`, `contentType: application/json` and `data = {"response": message}`
//!   (`handlers.py:26-33`). The hermes chatbot handler returns `(200, "OK")` and `(500, "error")`
//!   on a body it cannot parse (`adapter.py:738-741`), which is what this port sends — with
//!   `message` left `""` because `CallbackHandler.raw_process` never sets it.
//! - ACK ordering: the library awaits `handler.raw_process` and then writes the ACK
//!   (`stream.py:129-136`); the hermes handler deliberately does its work in a background task so
//!   the ACK is not delayed (`adapter.py:723-741`). This port writes the ACK **before** handing
//!   the event to the caller.
//! - Routing: `SYSTEM` topic `disconnect` ACKs and then closes the socket so the outer loop
//!   reopens (`stream.py:105-106,117-119`); an unregistered `CALLBACK` topic and an unknown frame
//!   `type` are logged and **not** ACKed (`stream.py:130-134`).
//! - Reconnect/backoff, exactly the library's numbers: a failed open connection retries after
//!   10 s (`stream.py:67-70`), a network/closed error after 10 s (`stream.py:82-86`), any other
//!   exception after 3 s (`stream.py:87-90`), and `start_forever`'s outer restart waits 3 s
//!   (`stream.py:146`). Close codes follow the `websockets` classes the library catches: 1000
//!   (normal) and 1001 (going away) end the `async for` normally, so the next iteration reopens
//!   immediately; every other code surfaces as `ConnectionClosedError` and pays the 10 s wait.
//!   See [`CloseOutcome`] / [`close_outcome_for_code`].
//! - Dedup: a repeated `messageId` is dispatched once (`adapter.py:462-463`) through a
//!   size-1000 set (`adapter.py:215`).
//! - Chatbot payload: `ChatbotMessage.from_dict` (`chatbot.py:190-252`) — conversation id/type,
//!   sender id/nick/staff id, `msgtype`, `text.content`, rich-text items, `atUsers`,
//!   `sessionWebhook`, `robotCode`, `createAt`, unknown fields kept as extensions. Text is
//!   `text.content` stripped, else the rich-text `text`/`content` items joined with a space, else
//!   the `audio`/`file` fallbacks (`chatbot.py:304-313`, `inbound.py:88-105`).
//! - Reply: `POST {sessionWebhook}` with `{"msgtype":"markdown","markdown":{"title":…,"text":…}}`
//!   (`adapter.py:529-537`), the text truncated to 20000 characters (`adapter.py:64`) and
//!   normalised by hermes' renderer workaround (`adapter.py:698-708`). A response with status
//!   `< 300` is success and gets a fresh 12-hex-character id (`adapter.py:532-535`); anything else
//!   is a truthful failure carrying the status and the first 200 characters of the body.
//!
//! # Behaviour your host must know about
//!
//! - **Keepalive is not reachable here.** The library pings the WebSocket every 60 s at the
//!   protocol level (`stream.py:94-100`). [`crate::socket::LineSocket`] carries text frames only,
//!   so that ping is owned by whatever implements the socket ([`crate::socket::WsSocket`] or the
//!   host); [`KEEPALIVE_PING_SECS`] is exported for a layer that can send it.
//! - **There is no `/sys/ping` frame in this library.** The only system topic it names is
//!   `disconnect` (`frames.py:164`, used at `stream.py:117`); every other `SYSTEM` frame —
//!   whatever its topic — is simply ACKed `200`/`"OK"` (`handlers.py:66-77`).
//! - **A malformed frame is skipped, not fatal.** In the reference, `json.loads` at
//!   `stream.py:78` raising inside `start()` lands in the catch-all at `stream.py:87-90` and tears
//!   the whole connection down for one bad frame. Dropping the connection would also drop the
//!   ACKs of every good frame behind it, so this port ignores the frame and keeps the socket.
//! - **A close code is not observable through [`crate::socket::LineSocket`].** `Ok(None)` from
//!   `next_line` is treated as a clean close (reopen immediately) and `Err` as the
//!   `ConnectionClosedError` path (10 s). [`close_outcome_for_code`] maps real codes for a host
//!   that can see them.
//! - **A failed reopen is reported, not retried silently.** The library loops forever, waiting
//!   10 s per attempt (`stream.py:63-70`). [`DingTalkStream::next_event`] returns the error so the
//!   host's own retry loop stays in charge; [`CloseOutcome::OpenFailed`] carries the reference's
//!   10 s for a host that prefers the library's cadence.
//! - The reference's own `RECONNECT_BACKOFF = [2, 5, 10, 30, 60]` ladder and its circuit breaker
//!   (`adapter.py:65-67,281-322`) sit *outside* `start()`, around errors that escape the SDK's
//!   internal retry loop. This port is the SDK's loop, so those are not ported — the SDK's numbers
//!   above are.
//!
//! # Omitted
//!
//! - Card / AI-card / emotion APIs, media (`downloadCode`) resolution, the `delegate` topic and
//!   the `hostingContext` / `conversationMsgContext` fields (`chatbot.py:237-249`) — they answer
//!   through OpenAPI, not the stream.
//! - The `card` / `interactiveCard` / rich-text item media promotions of `inbound.py:48-70,108-154`;
//!   only the plain `msgtype` → [`MessageType`] mapping is kept.
//! - `ChatbotHandler.reply_text` / `reply_markdown` (chatbot.py:618-676): their `at.atUserIds`
//!   block is not sent, because the adapter's `send()` path does not send it either
//!   (`adapter.py:529`).
//! - Group gating (`require_mention`, `allowed_chats`, mention patterns — `adapter.py:383-427`):
//!   access decisions belong to the host.
//!
//! The HTTP half goes through [`crate::transport::Transport`] and the frames through
//! [`crate::socket::LineSocket`], so both halves are testable against scripted doubles.

use serde_json::{Map, Value, json};
use std::collections::{BTreeMap, HashSet, VecDeque};
use std::sync::{Arc, Mutex, MutexGuard};
use std::time::Duration;

use crate::event::{MessageEvent, MessageType, Platform, SessionSource, platform_names};
use crate::platform::SendResult;
use crate::socket::{SharedLineSocket, SocketError, WsSocket};
use crate::transport::{HttpRequest, SharedTransport};

// ---------------------------------------------------------------------------------------------
// Constants — every one cites the reference line that defines it.
// ---------------------------------------------------------------------------------------------

/// `OPEN_CONNECTION_API`: `DINGTALK_OPENAPI_ENDPOINT` (default `https://api.dingtalk.com`,
/// `utils.py:8-10`) plus `/v1.0/gateway/connections/open` (`stream.py:28`).
pub const OPEN_CONNECTION_URL: &str =
    concat!("https://api.dingtalk.com", "/v1.0/gateway/connections/open");

/// `VERSION_STRING`'s value in the SDK hermes pins (`adapter.py:329-330`:"dingtalk-stream==0.24.3").
pub const SDK_VERSION: &str = "0.24.3";

/// The chatbot callback topic `ChatbotMessage.TOPIC` (`chatbot.py:159`) — the one topic hermes
/// registers (`adapter.py:247`).
pub const CHATBOT_TOPIC: &str = "/v1.0/im/bot/messages/get";

/// `ChatbotMessage.DELEGATE_TOPIC` (`chatbot.py:160`). Declared because it is part of the wire
/// vocabulary; this port registers no handler for it.
pub const CHATBOT_DELEGATE_TOPIC: &str = "/v1.0/im/bot/messages/delegate";

/// Frame `type` discriminators: `EventMessage.TYPE` (`frames.py:89`), `CallbackMessage.TYPE`
/// (`frames.py:126`), `SystemMessage.TYPE` (`frames.py:163`).
pub const FRAME_TYPE_SYSTEM: &str = "SYSTEM";
/// See [`FRAME_TYPE_SYSTEM`].
pub const FRAME_TYPE_EVENT: &str = "EVENT";
/// See [`FRAME_TYPE_SYSTEM`].
pub const FRAME_TYPE_CALLBACK: &str = "CALLBACK";

/// `SystemMessage.TOPIC_DISCONNECT` (`frames.py:164`) — the only system topic that acts.
pub const TOPIC_DISCONNECT: &str = "disconnect";

/// `Headers.CONTENT_TYPE_APPLICATION_JSON` (`frames.py:5`).
pub const CONTENT_TYPE_APPLICATION_JSON: &str = "application/json";

/// `AckMessage.STATUS_OK` (`frames.py:222`).
pub const ACK_CODE_OK: u16 = 200;
/// `AckMessage.STATUS_BAD_REQUEST` (`frames.py:223`).
pub const ACK_CODE_BAD_REQUEST: u16 = 400;
/// `AckMessage.STATUS_NOT_IMPLEMENT` (`frames.py:224`).
pub const ACK_CODE_NOT_IMPLEMENT: u16 = 404;
/// `AckMessage.STATUS_SYSTEM_EXCEPTION` (`frames.py:225`).
pub const ACK_CODE_SYSTEM_EXCEPTION: u16 = 500;

/// Wait before retrying after `open_connection` returned nothing (`stream.py:69`).
pub const OPEN_CONNECTION_RETRY_SECS: u64 = 10;
/// Wait after `ConnectionClosedError` / `CancelledError` (`stream.py:85`).
pub const NETWORK_RETRY_SECS: u64 = 10;
/// Wait after any other exception (`stream.py:88`).
pub const UNKNOWN_RETRY_SECS: u64 = 3;
/// `start_forever`'s wait between restarts (`stream.py:146`).
pub const START_FOREVER_RETRY_SECS: u64 = 3;
/// `keepalive(ping_interval=60)` (`stream.py:94`). Not sent by this module — see the module docs.
pub const KEEPALIVE_PING_SECS: u64 = 60;

/// `MAX_MESSAGE_LENGTH` the adapter truncates a webhook reply to (`adapter.py:64`).
pub const MAX_REPLY_CHARS: usize = 20000;

/// Markdown title the adapter hard-codes for webhook replies (`adapter.py:529`). The reference
/// puts its own assistant name there; this port puts `VIKRAM`.
pub const REPLY_TITLE: &str = "VIKRAM";

/// `MessageDeduplicator(max_size=1000)` (`adapter.py:215`).
pub const DEDUP_CAPACITY: usize = 1000;

/// Metadata key: the callback's `sessionWebhook`, needed to answer in this conversation.
pub const META_SESSION_WEBHOOK: &str = "session_webhook";
/// Metadata key: the bot's `robotCode`.
pub const META_ROBOT_CODE: &str = "robot_code";
/// Metadata key: the callback's `createAt` in epoch milliseconds.
pub const META_CREATE_AT: &str = "create_at";
/// Metadata key: `conversationType` as sent (`"1"` dm, `"2"` group).
pub const META_CONVERSATION_TYPE: &str = "conversation_type";
/// Metadata key: `isInAtList` — the structural "the bot was mentioned" signal.
pub const META_IS_IN_AT_LIST: &str = "is_in_at_list";
/// Metadata key: `senderStaffId`, the id the robot APIs address.
pub const META_SENDER_STAFF_ID: &str = "sender_staff_id";

// ---------------------------------------------------------------------------------------------
// Credentials and the open-connection request.
// ---------------------------------------------------------------------------------------------

/// `Credential` (`credential.py:1-4`).
///
/// Deliberately not `Debug`: it holds the app secret, and this type ends up in host logs.
#[derive(Clone, PartialEq, Eq)]
pub struct Credential {
    /// The app's `AppKey`, sent as the body's `clientId` (`stream.py:163`).
    pub client_id: String,
    /// The app's `AppSecret`, sent as the body's `clientSecret` (`stream.py:164`).
    pub client_secret: String,
}

impl Credential {
    /// A credential pair. Neither value is validated here; a wrong pair is refused by the
    /// open-connection endpoint.
    pub fn new(client_id: impl Into<String>, client_secret: impl Into<String>) -> Self {
        Self {
            client_id: client_id.into(),
            client_secret: client_secret.into(),
        }
    }
}

/// The `ua` body field: `'dingtalk-sdk-python/v%s-union' % VERSION_STRING` (`stream.py:166`).
pub fn sdk_ua_field() -> String {
    format!("dingtalk-sdk-python/v{SDK_VERSION}-union")
}

/// The user agent header (`stream.py:153-155`).
///
/// The reference interpolates `platform.python_version()` into the fourth slot. A Rust process has
/// no Python version to report, so this crate's version takes that slot and the rest of the
/// string — product token, SDK version, project URL — is unchanged.
pub fn user_agent_header() -> String {
    format!(
        "DingTalkStream/1.0 SDK/{SDK_VERSION} Rust/{} (+https://github.com/open-dingtalk/dingtalk-stream-sdk-python)",
        env!("CARGO_PKG_VERSION")
    )
}

/// `quote_plus` from `urllib.parse` with its default empty `safe` set, used to put the ticket in
/// the query string (`stream.py:73`).
///
/// Unreserved characters (`A-Za-z0-9_.-~`) are kept, a space becomes `+`, and every other byte —
/// of the UTF-8 encoding, so non-ASCII is escaped byte by byte — becomes `%XX`.
pub fn quote_plus(value: &str) -> String {
    let mut out = String::with_capacity(value.len());
    for byte in value.bytes() {
        match byte {
            b'A'..=b'Z' | b'a'..=b'z' | b'0'..=b'9' | b'-' | b'_' | b'.' | b'~' => {
                out.push(char::from(byte));
            }
            b' ' => out.push('+'),
            other => out.push_str(&format!("%{other:02X}")),
        }
    }
    out
}

/// The `localIp` value `get_host_ip` computes (`stream.py:183-195`).
///
/// A UDP socket is "connected" to `8.8.8.8:80` (which sends nothing) so the kernel picks the
/// route's local address. Any failure yields `""`, exactly as the reference's `ip = ""` default
/// does.
pub fn detect_local_ip() -> String {
    let Ok(socket) = std::net::UdpSocket::bind("0.0.0.0:0") else {
        return String::new();
    };
    if socket.connect("8.8.8.8:80").is_err() {
        return String::new();
    }
    socket
        .local_addr()
        .map(|address| address.ip().to_string())
        .unwrap_or_default()
}

/// One entry of the open-connection `subscriptions` array (`stream.py:157-161`).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Subscription {
    /// The subscription's `type` field.
    pub kind: SubscriptionKind,
    /// The subscription's `topic` field (`*` for the EVENT subscription).
    pub topic: String,
}

/// `type` of a subscription: the two literals `stream.py:159-161` produces.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SubscriptionKind {
    /// `{"type": "EVENT", "topic": "*"}` — only when event handlers are registered
    /// (`stream.py:158-159`).
    Event,
    /// `{"type": "CALLBACK", "topic": …}` — one per registered callback topic
    /// (`stream.py:160-161`).
    Callback,
}

impl SubscriptionKind {
    /// The wire string: `"EVENT"` or `"CALLBACK"`.
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Event => FRAME_TYPE_EVENT,
            Self::Callback => FRAME_TYPE_CALLBACK,
        }
    }
}

impl Subscription {
    /// A callback-topic subscription.
    pub fn callback(topic: impl Into<String>) -> Self {
        Self {
            kind: SubscriptionKind::Callback,
            topic: topic.into(),
        }
    }

    /// The catch-all event subscription (`stream.py:159`).
    pub fn events_all() -> Self {
        Self {
            kind: SubscriptionKind::Event,
            topic: "*".to_string(),
        }
    }

    /// The wire object: `{"type": …, "topic": …}`.
    pub fn to_value(&self) -> Value {
        json!({ "type": self.kind.as_str(), "topic": self.topic })
    }
}

// ---------------------------------------------------------------------------------------------
// Frames.
// ---------------------------------------------------------------------------------------------

/// `Headers` (`frames.py:4-85`): the camelCase header block every frame carries.
///
/// `to_value` reproduces `Headers.to_dict` (`frames.py:41-55`), which matters for ACKs: the echo
/// is `extensions` plus the six base names, `time` is written as a **string**, and the `event*`
/// fields — parsed but never written by `to_dict` — are dropped from the echo.
#[derive(Debug, Clone, Default, PartialEq)]
pub struct FrameHeaders {
    /// `appId`.
    pub app_id: Option<String>,
    /// `connectionId`.
    pub connection_id: Option<String>,
    /// `contentType` — `application/json` on ACKs (`handlers.py:31`).
    pub content_type: Option<String>,
    /// `messageId` — echoed so the server can match the ACK (`handlers.py:30`).
    pub message_id: Option<String>,
    /// `topic`.
    pub topic: Option<String>,
    /// `time`, an epoch-millisecond integer parsed the way `int(value)` parses it
    /// (`frames.py:72`).
    pub time: Option<i64>,
    /// `eventBornTime` (`frames.py:73-74`).
    pub event_born_time: Option<i64>,
    /// `eventCorpId` (`frames.py:75-76`).
    pub event_corp_id: Option<String>,
    /// `eventId` (`frames.py:77-78`).
    pub event_id: Option<String>,
    /// `eventType` (`frames.py:79-80`).
    pub event_type: Option<String>,
    /// `eventUnifiedAppId` (`frames.py:81-82`).
    pub event_unified_app_id: Option<String>,
    /// Every header this port does not name (`frames.py:83-84`), echoed back verbatim
    /// (`frames.py:42`).
    pub extensions: BTreeMap<String, Value>,
}

impl FrameHeaders {
    /// `Headers.from_dict` (`frames.py:57-85`). A non-object yields the default headers.
    pub fn from_value(value: &Value) -> Self {
        let mut headers = Self::default();
        let Some(object) = value.as_object() else {
            return headers;
        };
        for (name, value) in object {
            match name.as_str() {
                "appId" => headers.app_id = string_of(value),
                "connectionId" => headers.connection_id = string_of(value),
                "contentType" => headers.content_type = string_of(value),
                "messageId" => headers.message_id = string_of(value),
                "topic" => headers.topic = string_of(value),
                "time" => headers.time = int_of(value),
                "eventBornTime" => headers.event_born_time = int_of(value),
                "eventCorpId" => headers.event_corp_id = string_of(value),
                "eventId" => headers.event_id = string_of(value),
                "eventType" => headers.event_type = string_of(value),
                "eventUnifiedAppId" => headers.event_unified_app_id = string_of(value),
                _ => {
                    headers.extensions.insert(name.clone(), value.clone());
                }
            }
        }
        headers
    }

    /// `Headers.to_dict` (`frames.py:41-55`).
    pub fn to_value(&self) -> Value {
        let mut object = Map::new();
        for (name, value) in &self.extensions {
            object.insert(name.clone(), value.clone());
        }
        if let Some(value) = &self.app_id {
            object.insert("appId".to_string(), json!(value));
        }
        if let Some(value) = &self.connection_id {
            object.insert("connectionId".to_string(), json!(value));
        }
        if let Some(value) = &self.content_type {
            object.insert("contentType".to_string(), json!(value));
        }
        if let Some(value) = &self.message_id {
            object.insert("messageId".to_string(), json!(value));
        }
        if let Some(value) = &self.topic {
            object.insert("topic".to_string(), json!(value));
        }
        if let Some(value) = self.time {
            // `'time': str(self.time)` — a string on the wire even though it was an integer
            // (frames.py:53-54).
            object.insert("time".to_string(), json!(value.to_string()));
        }
        Value::Object(object)
    }
}

/// One inbound frame — the shared envelope of `EventMessage` / `CallbackMessage` /
/// `SystemMessage` (`frames.py:88-218`).
#[derive(Debug, Clone, Default, PartialEq)]
pub struct Frame {
    /// `type`. Empty when the frame carried none, which is how the reference's
    /// `json_message.get('type', '')` sees it (`stream.py:112`).
    pub kind: String,
    /// `specVersion` (`frames.py:111-112`).
    pub spec_version: Option<String>,
    /// `headers` (`frames.py:117-118`).
    pub headers: FrameHeaders,
    /// `data` after `json.loads` (`frames.py:121-122`); [`Value::Null`] when absent or unparsable.
    pub data: Value,
    /// Every top-level field the frame model does not name (`frames.py:119-120`).
    pub extensions: BTreeMap<String, Value>,
}

impl Frame {
    /// Parse one socket line. `None` for anything that is not a JSON object — the caller skips it.
    pub fn parse(raw: &str) -> Option<Self> {
        let value: Value = serde_json::from_str(raw).ok()?;
        value.is_object().then(|| Self::from_value(&value))
    }

    /// The `from_dict` every frame class shares (`frames.py:106-123`). A non-object yields the
    /// default frame.
    pub fn from_value(value: &Value) -> Self {
        let mut frame = Self::default();
        let Some(object) = value.as_object() else {
            return frame;
        };
        for (name, value) in object {
            match name.as_str() {
                "specVersion" => frame.spec_version = string_of(value),
                "data" => frame.data = decode_data(value),
                "type" => frame.kind = string_of(value).unwrap_or_default(),
                "headers" => frame.headers = FrameHeaders::from_value(value),
                _ => {
                    frame.extensions.insert(name.clone(), value.clone());
                }
            }
        }
        frame
    }
}

/// The `data` field: a JSON **string** holding JSON, which `from_dict` parses (`frames.py:121-122`).
///
/// An already-parsed object is kept as-is — that is what the hermes handler's
/// `json.loads(message.data) if isinstance(message.data, str) else message.data` does
/// (`adapter.py:726`). A string that is not JSON becomes [`Value::Null`] instead of raising, so a
/// malformed body is answerable with a 500 ACK rather than a torn connection.
fn decode_data(value: &Value) -> Value {
    match value {
        Value::String(text) => serde_json::from_str(text).unwrap_or(Value::Null),
        other => other.clone(),
    }
}

/// A string header as an owned `String`; numbers are accepted because a mirror may quote neither.
fn string_of(value: &Value) -> Option<String> {
    match value {
        Value::String(text) => Some(text.clone()),
        Value::Number(number) => Some(number.to_string()),
        Value::Bool(flag) => Some(flag.to_string()),
        _ => None,
    }
}

/// An integer header parsed with `int()`-like tolerance: a JSON integer or a numeric string.
fn int_of(value: &Value) -> Option<i64> {
    match value {
        Value::Number(number) => number
            .as_i64()
            .or_else(|| number.as_u64().map(|value| value as i64)),
        Value::String(text) => text.trim().parse::<i64>().ok(),
        _ => None,
    }
}

// ---------------------------------------------------------------------------------------------
// ACKs.
// ---------------------------------------------------------------------------------------------

/// `AckMessage` (`frames.py:221-239`).
#[derive(Debug, Clone, PartialEq)]
pub struct AckMessage {
    /// One of the `ACK_CODE_*` constants (`frames.py:222-225`).
    pub code: u16,
    /// The echoed headers: `messageId` + `contentType` per handler (`handlers.py:30-31`).
    pub headers: FrameHeaders,
    /// `message`. Left empty for callback ACKs, because `CallbackHandler.raw_process` never sets it
    /// (`handlers.py:26-33`).
    pub message: String,
    /// `data`, serialized to a JSON **string** on the wire (`frames.py:238`).
    pub data: Value,
}

impl AckMessage {
    /// `AckMessage.to_dict` (`frames.py:233-239`). `data` is stringified, not nested.
    pub fn to_value(&self) -> Value {
        json!({
            "code": self.code,
            "headers": self.headers.to_value(),
            "message": self.message,
            "data": serde_json::to_string(&self.data).unwrap_or_else(|_| "null".to_string()),
        })
    }

    /// The ACK for a `SYSTEM` frame: `200`, `"OK"`, frame data echoed (`handlers.py:66-77`).
    pub fn for_system(frame: &Frame) -> Self {
        Self {
            code: ACK_CODE_OK,
            headers: ack_headers(frame),
            message: "OK".to_string(),
            data: frame.data.clone(),
        }
    }

    /// The ACK for an `EVENT` frame with no registered event handler — the library's default
    /// `(STATUS_NOT_IMPLEMENT, 'not implement')` (`handlers.py:44-55`).
    pub fn for_default_event(frame: &Frame) -> Self {
        Self {
            code: ACK_CODE_NOT_IMPLEMENT,
            headers: ack_headers(frame),
            message: "not implement".to_string(),
            data: frame.data.clone(),
        }
    }

    /// The ACK for a `CALLBACK` frame: `code` plus `data = {"response": message}`
    /// (`handlers.py:26-33`).
    pub fn for_callback(frame: &Frame, code: u16, message: &str) -> Self {
        Self {
            code,
            headers: ack_headers(frame),
            message: String::new(),
            data: json!({ "response": message }),
        }
    }
}

/// The headers every handler puts on its ACK: the frame's `messageId` and
/// `contentType: application/json` (`handlers.py:30-31`).
fn ack_headers(frame: &Frame) -> FrameHeaders {
    FrameHeaders {
        message_id: frame.headers.message_id.clone(),
        content_type: Some(CONTENT_TYPE_APPLICATION_JSON.to_string()),
        ..FrameHeaders::default()
    }
}

// ---------------------------------------------------------------------------------------------
// Chatbot payload.
// ---------------------------------------------------------------------------------------------

/// `AtUser` (`chatbot.py:18-43`).
#[derive(Debug, Clone, Default, PartialEq)]
pub struct AtUser {
    /// `dingtalkId`.
    pub dingtalk_id: Option<String>,
    /// `staffId`.
    pub staff_id: Option<String>,
    /// Unlisted fields (`chatbot.py:32-33`).
    pub extensions: BTreeMap<String, Value>,
}

impl AtUser {
    /// `AtUser.from_dict` (`chatbot.py:24-35`).
    pub fn from_value(value: &Value) -> Self {
        let mut user = Self::default();
        let Some(object) = value.as_object() else {
            return user;
        };
        for (name, value) in object {
            match name.as_str() {
                "dingtalkId" => user.dingtalk_id = string_of(value),
                "staffId" => user.staff_id = string_of(value),
                _ => {
                    user.extensions.insert(name.clone(), value.clone());
                }
            }
        }
        user
    }
}

/// `TextContent` (`chatbot.py:46-71`).
#[derive(Debug, Clone, Default, PartialEq)]
pub struct TextContent {
    /// `content`, verbatim.
    pub content: Option<String>,
    /// Unlisted fields (`chatbot.py:62-63`).
    pub extensions: BTreeMap<String, Value>,
}

impl TextContent {
    /// `TextContent.from_dict` (`chatbot.py:56-65`).
    pub fn from_value(value: &Value) -> Self {
        let mut content = Self::default();
        let Some(object) = value.as_object() else {
            return content;
        };
        for (name, value) in object {
            match name.as_str() {
                "content" => content.content = string_of(value),
                _ => {
                    content.extensions.insert(name.clone(), value.clone());
                }
            }
        }
        content
    }
}

/// `ImageContent` (`chatbot.py:74-91`).
#[derive(Debug, Clone, Default, PartialEq)]
pub struct ImageContent {
    /// `downloadCode`, resolved through OpenAPI (not ported — see the module docs).
    pub download_code: Option<String>,
}

impl ImageContent {
    /// `ImageContent.from_dict` (`chatbot.py:79-85`).
    pub fn from_value(value: &Value) -> Self {
        Self {
            download_code: value.get("downloadCode").and_then(string_of),
        }
    }
}

/// `RichTextContent` (`chatbot.py:94-112`).
#[derive(Debug, Clone, Default, PartialEq)]
pub struct RichTextContent {
    /// `richText`: items that carry either a `text` or a `downloadCode` member
    /// (`chatbot.py:103-106`, read at `chatbot.py:308-313`).
    pub rich_text_list: Vec<Value>,
}

impl RichTextContent {
    /// `RichTextContent.from_dict` (`chatbot.py:99-106`).
    pub fn from_value(value: &Value) -> Self {
        Self {
            rich_text_list: value
                .get("richText")
                .and_then(Value::as_array)
                .cloned()
                .unwrap_or_default(),
        }
    }
}

/// `ChatbotMessage` (`chatbot.py:158-332`) — the callback body's `data` object.
#[derive(Debug, Clone, Default, PartialEq)]
pub struct ChatbotMessage {
    /// `isInAtList`: the structural "the bot was mentioned" flag (`adapter.py:425`).
    pub is_in_at_list: Option<bool>,
    /// `sessionWebhook`: the per-message reply URL (`adapter.py:476-480`).
    pub session_webhook: Option<String>,
    /// `senderNick`.
    pub sender_nick: Option<String>,
    /// `robotCode`.
    pub robot_code: Option<String>,
    /// `sessionWebhookExpiredTime`, epoch milliseconds (`chatbot.py:203-204`).
    pub session_webhook_expired_time: Option<i64>,
    /// `msgId`.
    pub message_id: Option<String>,
    /// `senderId`.
    pub sender_id: Option<String>,
    /// `chatbotUserId`.
    pub chatbot_user_id: Option<String>,
    /// `conversationId` — the chat id (`adapter.py:467`).
    pub conversation_id: Option<String>,
    /// `isAdmin`.
    pub is_admin: Option<bool>,
    /// `createAt`, epoch milliseconds.
    ///
    /// The reference stores it un-coerced and `int()`s it in the adapter (`adapter.py:489-493`);
    /// this port parses it once, tolerantly.
    pub create_at: Option<i64>,
    /// `conversationType`: `"1"` dm, `"2"` group (`adapter.py:465`).
    pub conversation_type: Option<String>,
    /// `atUsers` (`chatbot.py:219-220`).
    pub at_users: Vec<AtUser>,
    /// `chatbotCorpId`.
    pub chatbot_corp_id: Option<String>,
    /// `senderCorpId`.
    pub sender_corp_id: Option<String>,
    /// `conversationTitle` — the group's name (`adapter.py:486`).
    pub conversation_title: Option<String>,
    /// `msgtype`: `text`, `richText`, `picture`, `audio`, `file`, …
    pub message_type: Option<String>,
    /// `text` for `msgtype: text` (`chatbot.py:229-230`).
    pub text: Option<TextContent>,
    /// `content` for `msgtype: picture` (`chatbot.py:231-232`).
    pub image_content: Option<ImageContent>,
    /// `content` for `msgtype: richText` (`chatbot.py:233-234`).
    pub rich_text_content: Option<RichTextContent>,
    /// `senderStaffId` — the id the robot APIs and `at` blocks use.
    pub sender_staff_id: Option<String>,
    /// Every field this port does not name (`chatbot.py:250-251`).
    pub extensions: BTreeMap<String, Value>,
}

impl ChatbotMessage {
    /// `ChatbotMessage.from_dict` (`chatbot.py:190-252`).
    ///
    /// The reference reads `d['text']` / `d['content']` as soon as it sees `msgtype` and would
    /// raise on a body that omits them; here the content is simply `None`, so a truncated callback
    /// is classified by the ACK instead of ending the connection.
    pub fn from_dict(value: &Value) -> Self {
        let mut message = Self::default();
        let Some(object) = value.as_object() else {
            return message;
        };
        for (name, value) in object {
            match name.as_str() {
                "isInAtList" => message.is_in_at_list = value.as_bool(),
                "sessionWebhook" => message.session_webhook = string_of(value),
                "senderNick" => message.sender_nick = string_of(value),
                "robotCode" => message.robot_code = string_of(value),
                "sessionWebhookExpiredTime" => message.session_webhook_expired_time = int_of(value),
                "msgId" => message.message_id = string_of(value),
                "senderId" => message.sender_id = string_of(value),
                "chatbotUserId" => message.chatbot_user_id = string_of(value),
                "conversationId" => message.conversation_id = string_of(value),
                "isAdmin" => message.is_admin = value.as_bool(),
                "createAt" => message.create_at = int_of(value),
                "conversationType" => message.conversation_type = string_of(value),
                "atUsers" => {
                    message.at_users = value
                        .as_array()
                        .map(|users| users.iter().map(AtUser::from_value).collect())
                        .unwrap_or_default();
                }
                "chatbotCorpId" => message.chatbot_corp_id = string_of(value),
                "senderCorpId" => message.sender_corp_id = string_of(value),
                "conversationTitle" => message.conversation_title = string_of(value),
                "msgtype" => {
                    message.message_type = string_of(value);
                    // The type decides which content block is populated (`chatbot.py:227-234`).
                    match message.message_type.as_deref() {
                        Some("text") => {
                            message.text = object.get("text").map(TextContent::from_value);
                        }
                        Some("picture") => {
                            message.image_content =
                                object.get("content").map(ImageContent::from_value);
                        }
                        Some("richText") => {
                            message.rich_text_content =
                                object.get("content").map(RichTextContent::from_value);
                        }
                        _ => {}
                    }
                }
                "senderStaffId" => message.sender_staff_id = string_of(value),
                _ => {
                    message.extensions.insert(name.clone(), value.clone());
                }
            }
        }
        message
    }

    /// `get_text_list` (`chatbot.py:304-313`) as hermes' `extract_text` renders it
    /// (`inbound.py:94-105`).
    ///
    /// Plain text is `text.content` trimmed. Otherwise the rich-text items are joined with a
    /// space. Otherwise the `audio` (speech recognition) and `file` (file name) fallbacks apply
    /// (`inbound.py:80-85`). Mentions are **not** stripped: the reference routes them structurally
    /// through `isInAtList` and keeps the literal `@handle` in the text (`inbound.py:103-105`).
    pub fn text(&self) -> String {
        let plain = self
            .text
            .as_ref()
            .and_then(|text| text.content.clone())
            .unwrap_or_default()
            .trim()
            .to_string();
        if !plain.is_empty() {
            return plain;
        }
        let rich = self
            .rich_text_content
            .as_ref()
            .map(|rich| {
                rich.rich_text_list
                    .iter()
                    .filter_map(rich_item_text)
                    .collect::<Vec<String>>()
                    .join(" ")
                    .trim()
                    .to_string()
            })
            .unwrap_or_default();
        if !rich.is_empty() {
            return rich;
        }
        self.empty_text_fallback()
    }

    /// The `_EMPTY_TEXT_FALLBACKS` arm of `inbound.py:80-85` for the types that still carry text.
    fn empty_text_fallback(&self) -> String {
        match self.message_type.as_deref() {
            Some("audio") => self.content_field("recognition").trim().to_string(),
            Some("file") => {
                let name = self.content_field("fileName").trim().to_string();
                if name.is_empty() {
                    String::new()
                } else {
                    format!("[文件] {name}")
                }
            }
            _ => String::new(),
        }
    }

    /// `extensions['content'][field]` as text — `_ext_field` (`inbound.py:73-75`).
    fn content_field(&self, field: &str) -> String {
        self.extensions
            .get("content")
            .and_then(|content| content.get(field))
            .and_then(Value::as_str)
            .unwrap_or_default()
            .to_string()
    }

    /// `conversationType == "2"` — the adapter's single group test (`adapter.py:465`).
    pub fn is_group(&self) -> bool {
        self.conversation_type.as_deref() == Some("2")
    }

    /// The chat id the adapter routes with: `conversation_id or sender_id` (`adapter.py:467`).
    pub fn chat_id(&self) -> String {
        self.conversation_id
            .clone()
            .filter(|id| !id.is_empty())
            .or_else(|| self.sender_id.clone())
            .unwrap_or_default()
    }

    /// Build the gateway event, or `None` when there is nothing to route.
    ///
    /// The adapter drops a message with no text and no media (`adapter.py:484-485`) and routes by
    /// `chat_id`; this port drops the same messages and, additionally, a message with neither a
    /// conversation nor a sender (nowhere to answer).
    pub fn to_event(&self) -> Option<MessageEvent> {
        let text = self.text();
        if text.trim().is_empty() {
            return None;
        }
        let sender_id = self.sender_id.clone().unwrap_or_default();
        let chat_id = self.chat_id();
        if chat_id.is_empty() && sender_id.is_empty() {
            return None;
        }
        let chat_id = if chat_id.is_empty() {
            sender_id.clone()
        } else {
            chat_id
        };
        // `sender_nick = sender_nick or sender_id` (adapter.py:466).
        let sender_nick = self
            .sender_nick
            .clone()
            .filter(|nick| !nick.is_empty())
            .unwrap_or_else(|| sender_id.clone());

        let mut source = SessionSource::new(dingtalk_platform(), chat_id);
        source.chat_type = if self.is_group() { "group" } else { "dm" }.to_string();
        source.chat_name = self.conversation_title.clone();
        source.user_id = (!sender_id.is_empty()).then(|| sender_id.clone());
        source.user_name = (!sender_nick.is_empty()).then(|| sender_nick.clone());
        source.user_id_alt = self.sender_staff_id.clone();
        source.message_id = self.message_id.clone();

        let mut event = MessageEvent::new(text, source);
        event.message_type = self.gateway_message_type();
        event.user_id = (!sender_id.is_empty()).then_some(sender_id);
        event.user_name = (!sender_nick.is_empty()).then_some(sender_nick);
        event.message_id = self.message_id.clone();
        if let Some(webhook) = &self.session_webhook {
            event
                .metadata
                .insert(META_SESSION_WEBHOOK.to_string(), json!(webhook));
        }
        if let Some(robot_code) = &self.robot_code {
            event
                .metadata
                .insert(META_ROBOT_CODE.to_string(), json!(robot_code));
        }
        if let Some(create_at) = self.create_at {
            event
                .metadata
                .insert(META_CREATE_AT.to_string(), json!(create_at));
        }
        if let Some(kind) = &self.conversation_type {
            event
                .metadata
                .insert(META_CONVERSATION_TYPE.to_string(), json!(kind));
        }
        if let Some(in_at_list) = self.is_in_at_list {
            event
                .metadata
                .insert(META_IS_IN_AT_LIST.to_string(), json!(in_at_list));
        }
        if let Some(staff_id) = &self.sender_staff_id {
            event
                .metadata
                .insert(META_SENDER_STAFF_ID.to_string(), json!(staff_id));
        }
        Some(event)
    }

    /// `msgtype` → [`MessageType`], the plain part of `inbound.py:132-154`.
    fn gateway_message_type(&self) -> MessageType {
        match self.message_type.as_deref() {
            Some("picture") | Some("image") => MessageType::Photo,
            Some("audio") | Some("voice") => MessageType::Voice,
            Some("file") => MessageType::Document,
            _ => MessageType::Text,
        }
    }
}

/// A rich-text item's text: `item["text"]` or `item["content"]` (`inbound.py:88-91`).
fn rich_item_text(item: &Value) -> Option<String> {
    let text = item
        .get("text")
        .and_then(Value::as_str)
        .filter(|text| !text.is_empty())
        .or_else(|| item.get("content").and_then(Value::as_str))?;
    (!text.is_empty()).then(|| text.to_string())
}

/// The gateway's platform handle for this adapter (`platform_names::DINGTALK`); the fallback arm
/// is unreachable because the constant is non-empty.
fn dingtalk_platform() -> Platform {
    Platform::new(platform_names::DINGTALK).unwrap_or_else(Platform::local)
}

// ---------------------------------------------------------------------------------------------
// Open-connection response and reconnect policy.
// ---------------------------------------------------------------------------------------------

/// What `POST /v1.0/gateway/connections/open` answered (`stream.py:181`) and the URL it yields.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ConnectionInfo {
    /// `endpoint`, e.g. `wss://xxx.dingtalk.com/connect`.
    pub endpoint: String,
    /// `ticket`, valid for one connection.
    pub ticket: String,
}

impl ConnectionInfo {
    /// Parse the response body. A body without a usable `endpoint` is an error, because Python's
    /// `KeyError`/empty-string endpoint would leave `websockets.connect` with a URL that cannot
    /// work either (`stream.py:73`).
    pub fn from_response(body: &str) -> Result<Self, DingTalkStreamError> {
        let value: Value = serde_json::from_str(body).map_err(|err| {
            DingTalkStreamError::OpenResponse(format!("response was not json: {err}"))
        })?;
        let endpoint = value
            .get("endpoint")
            .and_then(Value::as_str)
            .map(str::trim)
            .filter(|endpoint| !endpoint.is_empty())
            .ok_or_else(|| {
                DingTalkStreamError::OpenResponse("response carried no endpoint".to_string())
            })?;
        let ticket = value.get("ticket").and_then(Value::as_str).ok_or_else(|| {
            DingTalkStreamError::OpenResponse("response carried no ticket".to_string())
        })?;
        Ok(Self {
            endpoint: endpoint.to_string(),
            ticket: ticket.to_string(),
        })
    }

    /// `f'{endpoint}?ticket={quote_plus(ticket)}'` (`stream.py:73`).
    pub fn websocket_url(&self) -> String {
        format!("{}?ticket={}", self.endpoint, quote_plus(&self.ticket))
    }
}

/// Why a stream iteration ended, and the wait the reference puts before the next one.
///
/// Values are `stream.py`'s: a failed open connection waits 10 s (`stream.py:67-70`), a
/// `ConnectionClosedError`/`CancelledError` 10 s (`stream.py:82-86`), anything else 3 s
/// (`stream.py:87-90`). A clean close reopens at once — the `async for` simply ends and the
/// `while True` re-runs `open_connection` (`stream.py:63-79`).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum CloseOutcome {
    /// Close code 1000/1001, or an iteration that ended normally: reopen immediately.
    Clean,
    /// A `ConnectionClosedError` (any other close code) or a socket failure: wait 10 s.
    Network,
    /// `open_connection` produced nothing: wait 10 s (`stream.py:69`).
    OpenFailed,
    /// Any other exception: wait 3 s (`stream.py:88`).
    Unknown,
}

impl CloseOutcome {
    /// The wait before the next attempt.
    pub fn backoff(self) -> Duration {
        match self {
            Self::Clean => Duration::ZERO,
            Self::Network | Self::OpenFailed => Duration::from_secs(NETWORK_RETRY_SECS),
            Self::Unknown => Duration::from_secs(UNKNOWN_RETRY_SECS),
        }
    }

    /// Classify a `next_line` result. The line socket has no close code, so `Ok(None)` (the stream
    /// ended, i.e. the frame loop finished normally) is [`Self::Clean`] and any error is
    /// [`Self::Network`].
    pub fn from_socket_result(result: Result<Option<String>, SocketError>) -> Self {
        match result {
            Ok(None) => Self::Clean,
            Ok(Some(_)) => Self::Unknown,
            Err(_) => Self::Network,
        }
    }
}

/// The `websockets` close-code split the reference relies on: `ConnectionClosedOK` (which the
/// `async for` swallows as a normal end) for 1000 and 1001, `ConnectionClosedError` for the rest
/// (`stream.py:74-86`).
pub fn close_outcome_for_code(code: u16) -> CloseOutcome {
    match code {
        1000 | 1001 => CloseOutcome::Clean,
        _ => CloseOutcome::Network,
    }
}

// ---------------------------------------------------------------------------------------------
// Errors.
// ---------------------------------------------------------------------------------------------

/// Everything the stream transport can fail with.
#[derive(Debug, thiserror::Error)]
pub enum DingTalkStreamError {
    /// The open-connection POST never produced a response.
    #[error("dingtalk open-connection request failed: {0}")]
    Transport(String),
    /// The open-connection POST answered a non-2xx status (`raise_for_status`, `stream.py:177`).
    #[error("dingtalk open-connection returned HTTP {status}: {body}")]
    OpenStatus {
        /// The status the endpoint answered.
        status: u16,
        /// The first 200 characters of the body, for the log.
        body: String,
    },
    /// The open-connection response could not be used.
    #[error("dingtalk open-connection response was not usable: {0}")]
    OpenResponse(String),
    /// The WebSocket could not be opened, or a frame could not be written.
    #[error("dingtalk stream socket failed: {0}")]
    Socket(String),
    /// Nothing is connected, so there is nothing to read.
    #[error("dingtalk stream is not connected")]
    NotConnected,
}

// ---------------------------------------------------------------------------------------------
// Dedup.
// ---------------------------------------------------------------------------------------------

/// `MessageDeduplicator` (`adapter.py:215`, used at `adapter.py:462-463`): a bounded
/// first-seen set.
struct MessageDeduplicator {
    capacity: usize,
    seen: HashSet<String>,
    order: VecDeque<String>,
}

impl MessageDeduplicator {
    fn new(capacity: usize) -> Self {
        Self {
            capacity,
            seen: HashSet::new(),
            order: VecDeque::new(),
        }
    }

    /// `True` when `id` was already dispatched. An empty id is never a duplicate: the adapter
    /// substitutes a fresh uuid for a missing `msgId` (`adapter.py:461`).
    fn is_duplicate(&mut self, id: &str) -> bool {
        if id.is_empty() {
            return false;
        }
        if self.seen.contains(id) {
            return true;
        }
        self.seen.insert(id.to_string());
        self.order.push_back(id.to_string());
        while self.order.len() > self.capacity {
            if let Some(oldest) = self.order.pop_front() {
                self.seen.remove(&oldest);
            }
        }
        false
    }
}

// ---------------------------------------------------------------------------------------------
// The client.
// ---------------------------------------------------------------------------------------------

/// Builds the socket for a `wss://…?ticket=…` URL. A reconnect needs a *new* socket with a *new*
/// ticket, which is why the client holds a factory and not a socket.
pub type SocketFactory = Arc<dyn Fn(&str) -> Result<SharedLineSocket, SocketError> + Send + Sync>;

/// The production factory: [`WsSocket::connect`] over rustls.
pub fn ws_socket_factory() -> SocketFactory {
    Arc::new(|url: &str| WsSocket::connect(url).map(WsSocket::into_shared))
}

/// What one frame contributed to the loop.
enum FrameStep {
    /// A message for the host.
    Event(Box<MessageEvent>),
    /// Acked, ignored, or nothing this port understands — read the next frame.
    Continue,
    /// The server asked for a disconnect (`stream.py:117-119`); reopen after the outcome's wait.
    Reconnect(CloseOutcome),
}

/// The Stream Mode protocol client: open-connection request, frame routing, ACKs, reconnect.
///
/// Every inbound frame is read from a [`crate::socket::LineSocket`] and every HTTP call goes
/// through a [`crate::transport::Transport`], so the whole protocol is testable with scripted
/// doubles.
pub struct DingTalkStreamClient {
    credential: Credential,
    transport: SharedTransport,
    socket_factory: SocketFactory,
    open_url: String,
    local_ip: String,
    /// The `subscriptions` array sent on every open-connection (`stream.py:157-161`).
    pub subscriptions: Vec<Subscription>,
    socket: Mutex<Option<SharedLineSocket>>,
    dedup: Mutex<MessageDeduplicator>,
    endpoint: Mutex<Option<String>>,
}

impl DingTalkStreamClient {
    /// A client for `credential` over `transport`, with the production socket factory
    /// ([`ws_socket_factory`]) and the chatbot callback topic subscribed (`adapter.py:247`).
    ///
    /// [`detect_local_ip`] runs here, once; use [`Self::with_local_ip`] to state the address
    /// explicitly (a host behind NAT, a test).
    pub fn new(credential: Credential, transport: SharedTransport) -> Self {
        Self {
            credential,
            transport,
            socket_factory: ws_socket_factory(),
            open_url: OPEN_CONNECTION_URL.to_string(),
            local_ip: detect_local_ip(),
            subscriptions: vec![Subscription::callback(CHATBOT_TOPIC)],
            socket: Mutex::new(None),
            dedup: Mutex::new(MessageDeduplicator::new(DEDUP_CAPACITY)),
            endpoint: Mutex::new(None),
        }
    }

    /// Point the client at a mirror or a test double; a trailing `/` is dropped.
    pub fn with_open_url(mut self, url: impl Into<String>) -> Self {
        self.open_url = url.into().trim_end_matches('/').to_string();
        self
    }

    /// Override the `localIp` field. `None` sends `""`, like a failed `get_host_ip`
    /// (`stream.py:188-195`).
    pub fn with_local_ip(mut self, ip: Option<String>) -> Self {
        self.local_ip = ip.unwrap_or_default();
        self
    }

    /// Replace the socket factory (tests, or a host that owns its own TLS stack).
    pub fn with_socket_factory(mut self, factory: SocketFactory) -> Self {
        self.socket_factory = factory;
        self
    }

    /// Replace the whole subscription list.
    pub fn with_subscriptions(mut self, subscriptions: Vec<Subscription>) -> Self {
        self.subscriptions = subscriptions;
        self
    }

    /// Add a callback topic. The reference keys its handlers by topic (`callback_handler_map`,
    /// read at `stream.py:160-161`), so a topic already registered is not added twice.
    pub fn subscribe(&mut self, topic: impl Into<String>) {
        let topic = topic.into();
        let known = self.subscriptions.iter().any(|subscription| {
            subscription.kind == SubscriptionKind::Callback && subscription.topic == topic
        });
        if !known {
            self.subscriptions.push(Subscription::callback(topic));
        }
    }

    /// Add the `EVENT`/`*` subscription (`stream.py:158-159`). Inserted first, because the request
    /// builder emits event subscriptions before callback ones.
    pub fn subscribe_events(&mut self) {
        if !self
            .subscriptions
            .iter()
            .any(|subscription| subscription.kind == SubscriptionKind::Event)
        {
            self.subscriptions.insert(0, Subscription::events_all());
        }
    }

    /// The `POST /v1.0/gateway/connections/open` request: headers `stream.py:150-156`, body
    /// `stream.py:162-168`.
    ///
    /// `serde_json` sorts object keys, so the body's key order differs from the reference's
    /// literal while the JSON itself is identical.
    pub fn open_connection_request(&self) -> HttpRequest {
        let body = json!({
            "clientId": self.credential.client_id,
            "clientSecret": self.credential.client_secret,
            "subscriptions": subscriptions_value(&self.subscriptions),
            "ua": sdk_ua_field(),
            "localIp": self.local_ip,
        });
        HttpRequest::json(self.open_url.clone(), &body)
            .header("Accept", CONTENT_TYPE_APPLICATION_JSON)
            .header("User-Agent", user_agent_header())
    }

    /// The current WebSocket URL, once a connection has been opened.
    pub fn endpoint(&self) -> Option<String> {
        lock(&self.endpoint).clone()
    }

    /// Is a socket held right now?
    pub fn is_connected(&self) -> bool {
        lock(&self.socket).is_some()
    }

    /// Drop the socket without opening another.
    pub fn close(&self) {
        *lock(&self.socket) = None;
        *lock(&self.endpoint) = None;
    }

    /// POST the open-connection request, parse the answer and open the WebSocket
    /// (`stream.py:65-75,148-181`).
    pub async fn open_connection(&self) -> Result<ConnectionInfo, DingTalkStreamError> {
        let response = self
            .transport
            .execute(self.open_connection_request())
            .await
            .map_err(|err| DingTalkStreamError::Transport(err.to_string()))?;
        if !response.ok() {
            return Err(DingTalkStreamError::OpenStatus {
                status: response.status,
                body: truncate_chars(&response.body, 200),
            });
        }
        let info = ConnectionInfo::from_response(&response.body)?;
        let url = info.websocket_url();
        let socket = (self.socket_factory)(&url)
            .map_err(|err| DingTalkStreamError::Socket(err.to_string()))?;
        *lock(&self.socket) = Some(socket);
        *lock(&self.endpoint) = Some(url);
        Ok(info)
    }

    /// Read until a message event is available.
    ///
    /// Returns `Err(NotConnected)` when [`Self::open_connection`] has not run, and the reopen
    /// error when a closed socket could not be replaced (the reference would retry silently; the
    /// host owns the cadence here — see the module docs).
    pub async fn next_event(&self) -> Result<Option<MessageEvent>, DingTalkStreamError> {
        loop {
            let socket = lock(&self.socket)
                .clone()
                .ok_or(DingTalkStreamError::NotConnected)?;
            match socket.next_line().await {
                Ok(Some(raw)) => match self.handle_frame(&raw, &socket).await? {
                    FrameStep::Event(event) => return Ok(Some(*event)),
                    FrameStep::Continue => {}
                    FrameStep::Reconnect(outcome) => {
                        self.close();
                        wait(outcome).await;
                        self.open_connection().await?;
                    }
                },
                other => {
                    // Clean close → immediate reopen; a socket error → the 10 s network wait
                    // (stream.py:74-86).
                    let outcome = CloseOutcome::from_socket_result(other);
                    self.close();
                    wait(outcome).await;
                    self.open_connection().await?;
                }
            }
        }
    }

    /// Route one frame (`stream.py:110-137`): ACK it as its handler would, then say what the loop
    /// should do next.
    async fn handle_frame(
        &self,
        raw: &str,
        socket: &SharedLineSocket,
    ) -> Result<FrameStep, DingTalkStreamError> {
        let Some(frame) = Frame::parse(raw) else {
            // Documented deviation: unparsable JSON keeps the connection (see the module docs).
            return Ok(FrameStep::Continue);
        };
        match frame.kind.as_str() {
            FRAME_TYPE_SYSTEM => {
                send_ack(socket, &AckMessage::for_system(&frame)).await?;
                if frame.headers.topic.as_deref() == Some(TOPIC_DISCONNECT) {
                    // ACK first, then close so the loop reopens (stream.py:105-106,117-119).
                    Ok(FrameStep::Reconnect(CloseOutcome::Clean))
                } else {
                    Ok(FrameStep::Continue)
                }
            }
            FRAME_TYPE_EVENT => {
                send_ack(socket, &AckMessage::for_default_event(&frame)).await?;
                Ok(FrameStep::Continue)
            }
            FRAME_TYPE_CALLBACK => self.handle_callback(&frame, socket).await,
            // `unknown message, content=…` and no ACK (stream.py:133-134).
            _ => Ok(FrameStep::Continue),
        }
    }

    /// The chatbot callback path (`adapter.py:723-741`).
    async fn handle_callback(
        &self,
        frame: &Frame,
        socket: &SharedLineSocket,
    ) -> Result<FrameStep, DingTalkStreamError> {
        if frame.headers.topic.as_deref() != Some(CHATBOT_TOPIC) {
            // `unknown callback message topic` — logged and NOT acked (stream.py:130-132).
            return Ok(FrameStep::Continue);
        }
        if !frame.data.is_object() {
            // The body did not parse into the object `ChatbotMessage.from_dict` needs: answer
            // `(500, "error")` exactly as the hermes handler does (adapter.py:738-740).
            send_ack(
                socket,
                &AckMessage::for_callback(frame, ACK_CODE_SYSTEM_EXCEPTION, "error"),
            )
            .await?;
            return Ok(FrameStep::Continue);
        }
        let message = ChatbotMessage::from_dict(&frame.data);
        // ACK before dispatch (adapter.py:723-741: the handler ACKs immediately and processes in a
        // background task).
        send_ack(socket, &AckMessage::for_callback(frame, ACK_CODE_OK, "OK")).await?;
        let message_id = message.message_id.clone().unwrap_or_default();
        if lock(&self.dedup).is_duplicate(&message_id) {
            return Ok(FrameStep::Continue);
        }
        match message.to_event() {
            Some(mut event) => {
                // Keep the payload the fields came from, for adapter-specific handling.
                event.raw_message = Some(frame.data.clone());
                Ok(FrameStep::Event(Box::new(event)))
            }
            None => Ok(FrameStep::Continue),
        }
    }

    /// Turn an already-parsed callback body into an event, applying the same ACK rules.
    ///
    /// Exposed so a host that feeds frames from its own reader (a multiplexed socket, a replay)
    /// shares the parsing, dedup and ACK semantics instead of re-implementing them. `Ok` carries the
    /// event and the ACK to write; `Err` carries the ACK that explains why no event was produced
    /// (a 500 for a body that is not an object, a 200 for a duplicate or an empty message).
    pub fn parse_callback_body(
        &self,
        body: &Value,
    ) -> Result<(MessageEvent, AckMessage), Box<AckMessage>> {
        let frame = Frame {
            kind: FRAME_TYPE_CALLBACK.to_string(),
            headers: FrameHeaders {
                topic: Some(CHATBOT_TOPIC.to_string()),
                message_id: body.get("msgId").and_then(string_of),
                ..FrameHeaders::default()
            },
            data: body.clone(),
            ..Frame::default()
        };
        if !body.is_object() {
            return Err(Box::new(AckMessage::for_callback(
                &frame,
                ACK_CODE_SYSTEM_EXCEPTION,
                "error",
            )));
        }
        let message = ChatbotMessage::from_dict(body);
        let message_id = message.message_id.clone().unwrap_or_default();
        if lock(&self.dedup).is_duplicate(&message_id) {
            return Err(Box::new(AckMessage::for_callback(
                &frame,
                ACK_CODE_OK,
                "OK",
            )));
        }
        match message.to_event() {
            Some(event) => Ok((event, AckMessage::for_callback(&frame, ACK_CODE_OK, "OK"))),
            None => Err(Box::new(AckMessage::for_callback(
                &frame,
                ACK_CODE_OK,
                "OK",
            ))),
        }
    }

    /// Answer in this conversation: `POST {session_webhook}` with the adapter's markdown body
    /// (`adapter.py:529-537`).
    pub async fn reply(&self, session_webhook: &str, text: &str) -> SendResult {
        let body = json!({
            "msgtype": "markdown",
            "markdown": {
                "title": REPLY_TITLE,
                "text": normalize_markdown(&truncate_chars(text, MAX_REPLY_CHARS)),
            },
        });
        self.send_to_webhook(session_webhook, &body).await
    }

    /// `POST {session_webhook}` with a caller-supplied body — the escape hatch for the library's
    /// `reply_text` body (`chatbot.py:625-633`), an `at` block, or a media payload.
    ///
    /// The verdict follows the reference where it has one: a status `< 300` is success and gets a
    /// fresh 12-hex-character id (`adapter.py:532-535`), everything else is a failure carrying the
    /// status and the first 200 characters of the body (`adapter.py:536-537`, which only logs).
    /// The `error_kind` classification is this port's: 429 `rate_limited` (retryable), 5xx
    /// `transient` (retryable), 401/403 `forbidden`, 404 `not_found`, anything else `unknown`.
    pub async fn send_to_webhook(&self, session_webhook: &str, body: &Value) -> SendResult {
        if session_webhook.trim().is_empty() {
            return SendResult::failed(
                "dingtalk needs the callback's sessionWebhook to answer a conversation",
                Some("bad_format"),
            );
        }
        // `Accept: */*` is the header both library reply helpers send (`chatbot.py:621-624`).
        let request = HttpRequest::json(session_webhook.trim(), body).header("Accept", "*/*");
        let response = match self.transport.execute(request).await {
            Ok(response) => response,
            Err(err) => {
                return SendResult::failed(
                    format!("dingtalk session webhook request failed: {err}"),
                    Some("transient"),
                )
                .retryable(None);
            }
        };
        if response.status < 300 {
            return SendResult::ok(new_message_id());
        }
        let message = format!(
            "HTTP {}: {}",
            response.status,
            truncate_chars(&response.body, 200)
        );
        let kind = match response.status {
            429 => "rate_limited",
            500..=599 => "transient",
            401 | 403 => "forbidden",
            404 => "not_found",
            _ => "unknown",
        };
        let mut result = SendResult::failed(message, Some(kind));
        if response.status == 429 {
            result = result.retryable(response.retry_after);
        } else if response.status >= 500 {
            result = result.retryable(None);
        }
        result
    }
}

/// The `subscriptions` array: EVENT entries first, then CALLBACK entries in registration order
/// (`stream.py:157-161`).
fn subscriptions_value(subscriptions: &[Subscription]) -> Vec<Value> {
    let mut values: Vec<Value> = subscriptions
        .iter()
        .filter(|subscription| subscription.kind == SubscriptionKind::Event)
        .map(Subscription::to_value)
        .collect();
    values.extend(
        subscriptions
            .iter()
            .filter(|subscription| subscription.kind == SubscriptionKind::Callback)
            .map(Subscription::to_value),
    );
    values
}

/// Write one ACK frame (`stream.py:135-136`).
async fn send_ack(socket: &SharedLineSocket, ack: &AckMessage) -> Result<(), DingTalkStreamError> {
    let line = serde_json::to_string(&ack.to_value())
        .map_err(|err| DingTalkStreamError::Socket(err.to_string()))?;
    socket
        .send_line(&line)
        .await
        .map_err(|err| DingTalkStreamError::Socket(err.to_string()))
}

/// Sleep the outcome's wait, skipping a zero one (a clean close reopens at once).
async fn wait(outcome: CloseOutcome) {
    let delay = outcome.backoff();
    if !delay.is_zero() {
        tokio::time::sleep(delay).await;
    }
}

/// A poisoned lock must not fail a read: nothing under these locks can be observed half-updated.
fn lock<T>(guard: &Mutex<T>) -> MutexGuard<'_, T> {
    guard
        .lock()
        .unwrap_or_else(|poisoned| poisoned.into_inner())
}

/// The first `limit` characters, Python's `text[:limit]` (`adapter.py:529`).
fn truncate_chars(text: &str, limit: usize) -> String {
    text.chars().take(limit).collect()
}

/// A fresh message id: `uuid.uuid4().hex[:12]` (`adapter.py:535`) — 6 random bytes as hex.
fn new_message_id() -> String {
    let mut bytes = [0u8; 6];
    if getrandom::fill(&mut bytes).is_err() {
        return String::new();
    }
    bytes.iter().map(|byte| format!("{byte:02x}")).collect()
}

/// `_normalize_markdown` (`adapter.py:698-708`): the two DingTalk renderer workarounds — a blank
/// line before a numbered list that follows text, and dedented code fences.
pub fn normalize_markdown(text: &str) -> String {
    let lines: Vec<&str> = text.split('\n').collect();
    let mut out: Vec<String> = Vec::with_capacity(lines.len() + 4);
    for (index, line) in lines.iter().enumerate() {
        let previous = if index > 0 {
            lines[index - 1].trim()
        } else {
            ""
        };
        if !previous.is_empty() && is_numbered(line.trim()) && !is_numbered(previous) {
            out.push(String::new());
        }
        if line.trim_start().starts_with("```") {
            out.push(line.trim_start().to_string());
        } else {
            out.push((*line).to_string());
        }
    }
    out.join("\n")
}

/// `_NUMBERED_RE = re.compile(r"^\d+\.\s")` (`adapter.py:131`).
fn is_numbered(line: &str) -> bool {
    let mut digits = 0usize;
    let mut chars = line.chars();
    for ch in chars.by_ref() {
        if ch.is_ascii_digit() {
            digits += 1;
            continue;
        }
        return ch == '.' && digits > 0 && chars.next().is_some_and(char::is_whitespace);
    }
    false
}

// ---------------------------------------------------------------------------------------------
// The host-facing half.
// ---------------------------------------------------------------------------------------------

/// DingTalk Stream Mode, as a host uses it: [`Self::connect`], [`Self::next_event`],
/// [`Self::reply`].
///
/// The host wraps this into its own platform type; no [`crate::platform::PlatformAdapter`] impl is
/// provided because the reply target is the inbound message's `sessionWebhook`, not a chat id.
pub struct DingTalkStream {
    client: DingTalkStreamClient,
}

impl DingTalkStream {
    /// The chatbot stream for `credential`, POSTing over `transport` and reading frames from
    /// [`WsSocket`]s.
    pub fn new(credential: Credential, transport: SharedTransport) -> Self {
        Self {
            client: DingTalkStreamClient::new(credential, transport),
        }
    }

    /// Replace the socket factory; production is [`ws_socket_factory`].
    pub fn with_socket_factory(mut self, factory: SocketFactory) -> Self {
        self.client = self.client.with_socket_factory(factory);
        self
    }

    /// Use one already-open socket instead of a factory.
    ///
    /// Such a socket can be used exactly once: a reconnect needs a fresh one, so the factory this
    /// installs then fails with [`SocketError::Io`] and [`Self::next_event`] returns
    /// [`DingTalkStreamError::Socket`].
    pub fn with_socket(mut self, socket: SharedLineSocket) -> Self {
        let slot = Arc::new(Mutex::new(Some(socket)));
        let factory: SocketFactory = Arc::new(move |_url: &str| {
            lock(&slot).take().ok_or_else(|| {
                SocketError::Io("dingtalk stream socket was already used".to_string())
            })
        });
        self.client = self.client.with_socket_factory(factory);
        self
    }

    /// Override the `localIp` sent on the open-connection request.
    pub fn with_local_ip(mut self, ip: Option<String>) -> Self {
        self.client = self.client.with_local_ip(ip);
        self
    }

    /// Point at a mirror or a test double.
    pub fn with_open_url(mut self, url: impl Into<String>) -> Self {
        self.client = self.client.with_open_url(url);
        self
    }

    /// The subscription list the next open-connection will send (`stream.py:157-161`).
    pub fn subscriptions(&self) -> &[Subscription] {
        &self.client.subscriptions
    }

    /// The same list, writable.
    pub fn subscriptions_mut(&mut self) -> &mut Vec<Subscription> {
        &mut self.client.subscriptions
    }

    /// Register another callback topic.
    pub fn subscribe(&mut self, topic: impl Into<String>) {
        self.client.subscribe(topic);
    }

    /// Register the catch-all `EVENT` subscription (`stream.py:158-159`).
    pub fn subscribe_events(&mut self) {
        self.client.subscribe_events();
    }

    /// The protocol client, for a host that wants the raw open-connection request or endpoint.
    pub fn client(&self) -> &DingTalkStreamClient {
        &self.client
    }

    /// Open the stream: POST the open connection, then open the WebSocket over the ticket.
    pub async fn connect(&self) -> Result<ConnectionInfo, DingTalkStreamError> {
        self.client.open_connection().await
    }

    /// Block until the next chatbot message arrives, answering every frame on the way
    /// (`stream.py:60-137`).
    pub async fn next_event(&self) -> Result<Option<MessageEvent>, DingTalkStreamError> {
        self.client.next_event().await
    }

    /// Answer the message behind `session_webhook` with markdown (`adapter.py:529-537`).
    pub async fn reply(&self, session_webhook: &str, text: &str) -> SendResult {
        self.client.reply(session_webhook, text).await
    }

    /// Answer with a caller-supplied webhook body (text, `at` block, …).
    pub async fn send_to_webhook(&self, session_webhook: &str, body: &Value) -> SendResult {
        self.client.send_to_webhook(session_webhook, body).await
    }

    /// Drop the socket.
    pub fn close(&self) {
        self.client.close();
    }

    /// Is a socket held right now?
    pub fn is_connected(&self) -> bool {
        self.client.is_connected()
    }
}

// ---------------------------------------------------------------------------------------------
// Tests.
// ---------------------------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;
    use crate::socket::LineSocket;
    use crate::transport::{HttpMethod, HttpResponse, Transport, TransportError};
    use async_trait::async_trait;

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

        fn bodies(&self) -> Vec<Value> {
            self.requests().iter().map(body_of).collect()
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

    /// A request's body as JSON (`Value::Null` when it carried none).
    fn body_of(request: &HttpRequest) -> Value {
        request
            .body
            .as_deref()
            .and_then(|bytes| serde_json::from_slice::<Value>(bytes).ok())
            .unwrap_or(Value::Null)
    }

    /// A scripted [`LineSocket`]: replays frames, records what was written.
    struct MockSocket {
        frames: Mutex<VecDeque<Result<Option<String>, SocketError>>>,
        sent: Mutex<Vec<String>>,
    }

    impl MockSocket {
        fn new(frames: Vec<Result<Option<String>, SocketError>>) -> Arc<Self> {
            Arc::new(Self {
                frames: Mutex::new(frames.into()),
                sent: Mutex::new(Vec::new()),
            })
        }

        fn sent(&self) -> Vec<String> {
            lock(&self.sent).clone()
        }

        fn sent_json(&self) -> Vec<Value> {
            self.sent()
                .iter()
                .map(|line| serde_json::from_str::<Value>(line).unwrap_or(Value::Null))
                .collect()
        }
    }

    #[async_trait]
    impl LineSocket for MockSocket {
        async fn send_line(&self, line: &str) -> Result<(), SocketError> {
            lock(&self.sent).push(line.to_string());
            Ok(())
        }

        async fn next_line(&self) -> Result<Option<String>, SocketError> {
            match lock(&self.frames).pop_front() {
                Some(frame) => frame,
                None => Ok(None),
            }
        }
    }

    const CLIENT_ID: &str = "ding-client-id";
    const CLIENT_SECRET: &str = "ding-client-secret";

    fn open_ok(endpoint: &str, ticket: &str) -> Result<HttpResponse, TransportError> {
        Ok(HttpResponse {
            status: 200,
            body: json!({ "endpoint": endpoint, "ticket": ticket }).to_string(),
            retry_after: None,
        })
    }

    fn http(status: u16, body: &str) -> Result<HttpResponse, TransportError> {
        Ok(HttpResponse {
            status,
            body: body.to_string(),
            retry_after: None,
        })
    }

    /// A factory that hands out the scripted sockets in order and then refuses — which is how a
    /// test stops the client's reconnect loop without sleeping.
    fn factory(sockets: Vec<Arc<MockSocket>>) -> (SocketFactory, Arc<Mutex<Vec<String>>>) {
        let queue: Arc<Mutex<VecDeque<Arc<MockSocket>>>> = Arc::new(Mutex::new(sockets.into()));
        let urls: Arc<Mutex<Vec<String>>> = Arc::new(Mutex::new(Vec::new()));
        let urls_ref = Arc::clone(&urls);
        let factory: SocketFactory = Arc::new(move |url: &str| {
            lock(&urls_ref).push(url.to_string());
            match lock(&queue).pop_front() {
                Some(socket) => Ok(socket as SharedLineSocket),
                None => Err(SocketError::Io("no scripted socket left".to_string())),
            }
        });
        (factory, urls)
    }

    fn client(
        mock: &Arc<MockTransport>,
        sockets: Vec<Arc<MockSocket>>,
    ) -> (DingTalkStreamClient, Arc<Mutex<Vec<String>>>) {
        let (factory, urls) = factory(sockets);
        let client = DingTalkStreamClient::new(
            Credential::new(CLIENT_ID, CLIENT_SECRET),
            Arc::clone(mock) as SharedTransport,
        )
        .with_local_ip(Some("10.1.2.3".to_string()))
        .with_socket_factory(factory);
        (client, urls)
    }

    fn callback_frame(topic: &str, message_id: &str, data: Value) -> String {
        json!({
            "specVersion": "1.0",
            "type": "CALLBACK",
            "headers": {
                "appId": "app-1",
                "connectionId": "conn-1",
                "contentType": "application/json",
                "messageId": message_id,
                "time": "1690000000000",
                "topic": topic,
            },
            "data": data.to_string(),
        })
        .to_string()
    }

    fn text_callback(message_id: &str, text: &str) -> String {
        callback_frame(
            CHATBOT_TOPIC,
            message_id,
            json!({
                "conversationId": "cid-1",
                "conversationType": "1",
                "msgId": message_id,
                "senderId": "sender-1",
                "senderNick": "Ann",
                "senderStaffId": "staff-1",
                "sessionWebhook": "https://oapi.dingtalk.com/robot/sendBySession?session=abc",
                "robotCode": "robot-1",
                "createAt": 1_690_000_000_000_i64,
                "msgtype": "text",
                "text": { "content": format!("  {text}  ") },
            }),
        )
    }

    // ----- open connection ---------------------------------------------------------------

    #[tokio::test]
    async fn open_connection_request_matches_the_library() {
        let mock = MockTransport::new(vec![open_ok("wss://host/connect", "t-1")]);
        let (client, _) = client(&mock, Vec::new());
        let request = client.open_connection_request();

        assert_eq!(request.method, HttpMethod::Post);
        assert_eq!(request.url, OPEN_CONNECTION_URL);
        assert_eq!(
            request.headers,
            vec![
                ("Content-Type".to_string(), "application/json".to_string()),
                ("Accept".to_string(), "application/json".to_string()),
                ("User-Agent".to_string(), user_agent_header()),
            ]
        );
        // stream.py:150-156 — product token, SDK version, project URL.
        assert!(user_agent_header().starts_with("DingTalkStream/1.0 SDK/0.24.3 Rust/"));
        assert!(
            user_agent_header()
                .ends_with("(+https://github.com/open-dingtalk/dingtalk-stream-sdk-python)")
        );

        // stream.py:162-168 — the five body fields, nothing else.
        let body = body_of(&request);
        let object = body.as_object().cloned().unwrap_or_default();
        let mut keys: Vec<&String> = object.keys().collect();
        keys.sort();
        assert_eq!(
            keys,
            vec!["clientId", "clientSecret", "localIp", "subscriptions", "ua"]
        );
        assert_eq!(body["clientId"], json!(CLIENT_ID));
        assert_eq!(body["clientSecret"], json!(CLIENT_SECRET));
        assert_eq!(body["localIp"], json!("10.1.2.3"));
        // stream.py:166 — `dingtalk-sdk-python/v%s-union`.
        assert_eq!(body["ua"], json!("dingtalk-sdk-python/v0.24.3-union"));
        // The default subscription is the one topic hermes registers (adapter.py:247).
        assert_eq!(
            body["subscriptions"],
            json!([{ "type": "CALLBACK", "topic": "/v1.0/im/bot/messages/get" }])
        );
    }

    #[tokio::test]
    async fn event_subscription_is_emitted_before_callback_topics() {
        let mock = MockTransport::new(Vec::new());
        let (mut client, _) = client(&mock, Vec::new());
        client.subscribe("/v1.0/im/bot/messages/delegate");
        client.subscribe_events();
        // Idempotent: a second registration must not duplicate the EVENT entry.
        client.subscribe_events();

        let body = body_of(&client.open_connection_request());
        assert_eq!(
            body["subscriptions"],
            json!([
                { "type": "EVENT", "topic": "*" },
                { "type": "CALLBACK", "topic": "/v1.0/im/bot/messages/get" },
                { "type": "CALLBACK", "topic": "/v1.0/im/bot/messages/delegate" },
            ])
        );
    }

    #[tokio::test]
    async fn open_connection_parses_the_endpoint_and_ticket() {
        let mock = MockTransport::new(vec![open_ok("wss://host/connect", "a b+c/d=")]);
        let socket = MockSocket::new(Vec::new());
        let (client, urls) = client(&mock, vec![socket]);

        let info = client.open_connection().await;
        assert!(info.is_ok(), "a 200 with endpoint+ticket must open");
        let info = info.unwrap_or(ConnectionInfo {
            endpoint: String::new(),
            ticket: String::new(),
        });
        assert_eq!(info.endpoint, "wss://host/connect");
        assert_eq!(info.ticket, "a b+c/d=");
        assert!(client.is_connected());
        // stream.py:73 — the endpoint is used verbatim and the ticket is quote_plus'd.
        assert_eq!(
            lock(&urls).clone(),
            vec!["wss://host/connect?ticket=a+b%2Bc%2Fd%3D".to_string()]
        );
        assert_eq!(
            client.endpoint().as_deref(),
            Some("wss://host/connect?ticket=a+b%2Bc%2Fd%3D")
        );
    }

    #[test]
    fn quote_plus_matches_urllib() {
        // Unreserved characters survive; space is '+'; the rest is %XX over UTF-8.
        assert_eq!(quote_plus("abcXYZ019-_.~"), "abcXYZ019-_.~");
        assert_eq!(quote_plus("a b"), "a+b");
        assert_eq!(quote_plus("a+b/c=d&e"), "a%2Bb%2Fc%3Dd%26e");
        assert_eq!(quote_plus("é"), "%C3%A9");
        assert_eq!(quote_plus(""), "");
        assert_eq!(quote_plus("```"), "%60%60%60");
    }

    #[tokio::test]
    async fn open_connection_reports_http_failure() {
        let mock = MockTransport::new(vec![http(401, "{\"code\":\"invalidAuth\"}")]);
        let socket = MockSocket::new(Vec::new());
        let (client, urls) = client(&mock, vec![socket]);

        match client.open_connection().await {
            Err(DingTalkStreamError::OpenStatus { status, body }) => {
                assert_eq!(status, 401);
                assert!(body.contains("invalidAuth"));
            }
            other => assert!(other.is_ok(), "a 401 must not look like success"),
        }
        assert!(!client.is_connected());
        assert!(lock(&urls).is_empty(), "no socket without a ticket");
    }

    #[tokio::test]
    async fn open_connection_reports_a_broken_response() {
        for body in [
            "not json",
            "{}",
            "{\"ticket\": \"t\"}",
            "{\"endpoint\": \"  \", \"ticket\": \"t\"}",
            "{\"endpoint\": \"wss://host/connect\"}",
        ] {
            let mock = MockTransport::new(vec![http(200, body)]);
            let (client, urls) = client(&mock, vec![MockSocket::new(Vec::new())]);
            assert!(
                matches!(
                    client.open_connection().await,
                    Err(DingTalkStreamError::OpenResponse(_))
                ),
                "{body} must be refused"
            );
            assert!(lock(&urls).is_empty());
        }
    }

    // ----- frames and acks -----------------------------------------------------------------

    #[tokio::test]
    async fn system_frames_are_acked_and_only_disconnect_reconnects() {
        // This library has no `ping` topic; every system topic except `disconnect` is just acked
        // (handlers.py:66-77, stream.py:117-126).
        let ping = json!({
            "type": "SYSTEM",
            "headers": { "messageId": "sys-1", "topic": "/sys/ping", "time": "1690000000000" },
            "data": { "opaque": true },
        })
        .to_string();
        let socket = MockSocket::new(vec![Ok(Some(ping)), Ok(Some(text_callback("m1", "hi")))]);
        let mock = MockTransport::new(vec![open_ok("wss://host/connect", "t-1")]);
        let (client, urls) = client(&mock, vec![Arc::clone(&socket)]);
        assert!(client.open_connection().await.is_ok());

        let event = client.next_event().await.unwrap_or(None);
        assert_eq!(event.map(|event| event.text), Some("hi".to_string()));

        // The ping was acked; only one socket was ever opened.
        assert_eq!(socket.sent_json().len(), 2);
        assert_eq!(lock(&urls).len(), 1);
    }

    #[tokio::test]
    async fn system_ack_echoes_the_message_id_and_the_frame_data() {
        let system = json!({
            "type": "SYSTEM",
            "headers": {
                "messageId": "sys-9",
                "topic": "/sys/ping",
                "time": "1690000000000",
                "eventId": "keep-me-out",
                "custom": { "x": 1 },
            },
            "data": { "opaque": true },
        })
        .to_string();
        let socket = MockSocket::new(vec![Ok(Some(system)), Ok(Some(text_callback("m1", "hi")))]);
        let mock = MockTransport::new(vec![open_ok("wss://host/connect", "t-1")]);
        let (client, _) = client(&mock, vec![Arc::clone(&socket)]);
        assert!(client.open_connection().await.is_ok());
        let _ = client.next_event().await;

        let acks = socket.sent_json();
        let ack = acks.first().cloned().unwrap_or(Value::Null);
        assert_eq!(ack["code"], json!(200));
        assert_eq!(ack["message"], json!("OK"));
        // handlers.py:30-31 — the ACK carries a *fresh* header block holding only the frame's
        // messageId plus contentType. Nothing else of the frame's headers is echoed: not the
        // unknown `custom`, not the frame's `time`, and not the `event*` fields `Headers.to_dict`
        // never writes (frames.py:41-55).
        assert_eq!(
            ack["headers"],
            json!({ "messageId": "sys-9", "contentType": "application/json" })
        );
        assert_eq!(ack["data"], json!("{\"opaque\":true}"));
    }

    #[tokio::test]
    async fn system_disconnect_reopens_the_connection() {
        let disconnect = json!({
            "type": "SYSTEM",
            "headers": { "messageId": "sys-d", "topic": "disconnect" },
            "data": {},
        })
        .to_string();
        let first = MockSocket::new(vec![Ok(Some(disconnect))]);
        let second = MockSocket::new(vec![
            Ok(Some(text_callback("m1", "after"))),
            Ok(Some(text_callback("m2", "second"))),
        ]);
        let mock = MockTransport::new(vec![
            open_ok("wss://host/one", "t-1"),
            open_ok("wss://host/two", "t-2"),
        ]);
        let (client, urls) = client(&mock, vec![Arc::clone(&first), Arc::clone(&second)]);
        assert!(client.open_connection().await.is_ok());

        let event = client.next_event().await.unwrap_or(None);
        assert_eq!(event.map(|event| event.text), Some("after".to_string()));
        // The disconnect was acked before the socket was dropped.
        assert_eq!(first.sent_json().len(), 1);
        assert_eq!(first.sent_json()[0]["code"], json!(200));
        assert_eq!(
            lock(&urls).clone(),
            vec![
                "wss://host/one?ticket=t-1".to_string(),
                "wss://host/two?ticket=t-2".to_string(),
            ]
        );
        assert_eq!(mock.requests().len(), 2);
    }

    #[tokio::test]
    async fn callback_frame_is_acked_then_dispatched() {
        let socket = MockSocket::new(vec![Ok(Some(text_callback("m1", "hello")))]);
        let mock = MockTransport::new(vec![open_ok("wss://host/connect", "t-1")]);
        let (client, _) = client(&mock, vec![Arc::clone(&socket)]);
        assert!(client.open_connection().await.is_ok());

        let event = client
            .next_event()
            .await
            .unwrap_or(None)
            .unwrap_or_else(|| MessageEvent::new("", SessionSource::new(dingtalk_platform(), "")));
        assert_eq!(event.text, "hello");
        assert_eq!(event.source.chat_id, "cid-1");
        assert_eq!(event.source.chat_type, "dm");
        assert_eq!(event.source.user_id.as_deref(), Some("sender-1"));
        assert_eq!(event.source.user_name.as_deref(), Some("Ann"));
        assert_eq!(event.source.user_id_alt.as_deref(), Some("staff-1"));
        assert_eq!(event.message_id.as_deref(), Some("m1"));
        assert_eq!(
            event.metadata[META_SESSION_WEBHOOK],
            json!("https://oapi.dingtalk.com/robot/sendBySession?session=abc")
        );
        assert_eq!(event.metadata[META_ROBOT_CODE], json!("robot-1"));
        assert_eq!(event.metadata[META_CREATE_AT], json!(1_690_000_000_000_i64));
        // The payload the fields came from is kept on the event.
        assert!(event.raw_message.is_some());

        // handlers.py:26-33 — code, echoed messageId, contentType, empty message, and `data` as a
        // JSON *string* holding {"response": "OK"}.
        let acks = socket.sent_json();
        assert_eq!(acks.len(), 1);
        assert_eq!(acks[0]["code"], json!(200));
        assert_eq!(acks[0]["message"], json!(""));
        assert_eq!(acks[0]["headers"]["messageId"], json!("m1"));
        assert_eq!(acks[0]["headers"]["contentType"], json!("application/json"));
        let data = acks[0]["data"].as_str().unwrap_or_default();
        assert_eq!(
            serde_json::from_str::<Value>(data).unwrap_or(Value::Null),
            json!({ "response": "OK" })
        );
    }

    #[tokio::test]
    async fn callback_with_unparsable_data_is_acked_500() {
        let broken = json!({
            "type": "CALLBACK",
            "headers": { "messageId": "m-bad", "topic": CHATBOT_TOPIC },
            "data": "[1, 2, 3]",
        })
        .to_string();
        let socket = MockSocket::new(vec![
            Ok(Some(broken)),
            Ok(Some(text_callback("m1", "after"))),
        ]);
        let mock = MockTransport::new(vec![open_ok("wss://host/connect", "t-1")]);
        let (client, _) = client(&mock, vec![Arc::clone(&socket)]);
        assert!(client.open_connection().await.is_ok());

        let event = client.next_event().await.unwrap_or(None);
        assert_eq!(event.map(|event| event.text), Some("after".to_string()));

        let acks = socket.sent_json();
        assert_eq!(acks.len(), 2);
        // adapter.py:738-740 — (500, "error").
        assert_eq!(acks[0]["code"], json!(500));
        assert_eq!(
            serde_json::from_str::<Value>(acks[0]["data"].as_str().unwrap_or_default())
                .unwrap_or(Value::Null),
            json!({ "response": "error" })
        );
        assert_eq!(acks[1]["code"], json!(200));
    }

    #[tokio::test]
    async fn duplicate_message_id_is_dispatched_once() {
        let socket = MockSocket::new(vec![
            Ok(Some(text_callback("m1", "first"))),
            Ok(Some(text_callback("m1", "first"))),
            Ok(Some(text_callback("m2", "second"))),
        ]);
        let mock = MockTransport::new(vec![open_ok("wss://host/connect", "t-1")]);
        let (client, _) = client(&mock, vec![Arc::clone(&socket)]);
        assert!(client.open_connection().await.is_ok());

        let first = client.next_event().await.unwrap_or(None);
        assert_eq!(first.map(|event| event.text), Some("first".to_string()));
        // The duplicate is ACKed again (the server sent it, the server gets an answer) but not
        // dispatched: the next event is "second".
        let second = client.next_event().await.unwrap_or(None);
        assert_eq!(second.map(|event| event.text), Some("second".to_string()));
        assert_eq!(socket.sent_json().len(), 3);
    }

    #[tokio::test]
    async fn malformed_json_is_skipped_without_an_ack() {
        let socket = MockSocket::new(vec![
            Ok(Some("this is not json".to_string())),
            Ok(Some("[1,2,3]".to_string())),
            Ok(Some(text_callback("m1", "survived"))),
        ]);
        let mock = MockTransport::new(vec![open_ok("wss://host/connect", "t-1")]);
        let (client, _) = client(&mock, vec![Arc::clone(&socket)]);
        assert!(client.open_connection().await.is_ok());

        let event = client.next_event().await.unwrap_or(None);
        assert_eq!(event.map(|event| event.text), Some("survived".to_string()));
        assert_eq!(socket.sent_json().len(), 1, "only the good frame is acked");
    }

    #[tokio::test]
    async fn wrong_topic_and_unknown_type_frames_are_ignored_without_acks() {
        let card = callback_frame("/v1.0/card/instances/callback", "m-card", json!({}));
        let other = json!({ "type": "NOT_A_FRAME", "headers": {}, "data": "{}" }).to_string();
        let socket = MockSocket::new(vec![
            Ok(Some(card)),
            Ok(Some(other)),
            Ok(Some(text_callback("m1", "chat"))),
        ]);
        let mock = MockTransport::new(vec![open_ok("wss://host/connect", "t-1")]);
        let (client, _) = client(&mock, vec![Arc::clone(&socket)]);
        assert!(client.open_connection().await.is_ok());

        let event = client.next_event().await.unwrap_or(None);
        assert_eq!(event.map(|event| event.text), Some("chat".to_string()));
        // stream.py:130-134 — neither the unregistered topic nor the unknown type is acked.
        assert_eq!(socket.sent_json().len(), 1);
    }

    #[tokio::test]
    async fn event_frames_get_the_default_not_implemented_ack() {
        let event = json!({
            "type": "EVENT",
            "headers": { "messageId": "e-1", "topic": "/v1.0/event/leave", "eventId": "x" },
            "data": { "k": "v" },
        })
        .to_string();
        let socket = MockSocket::new(vec![Ok(Some(event)), Ok(Some(text_callback("m1", "chat")))]);
        let mock = MockTransport::new(vec![open_ok("wss://host/connect", "t-1")]);
        let (client, _) = client(&mock, vec![Arc::clone(&socket)]);
        assert!(client.open_connection().await.is_ok());
        let _ = client.next_event().await;

        let acks = socket.sent_json();
        // handlers.py:44-55 — no event handler registered ⇒ (404, 'not implement').
        assert_eq!(acks[0]["code"], json!(404));
        assert_eq!(acks[0]["message"], json!("not implement"));
        assert_eq!(acks[0]["headers"]["messageId"], json!("e-1"));
    }

    // ----- chatbot extraction --------------------------------------------------------------

    #[test]
    fn chatbot_extracts_text_dm_fields() {
        let body = json!({
            "conversationId": "cid-9",
            "conversationTitle": "Team",
            "conversationType": "1",
            "msgId": "m-9",
            "senderId": "u-9",
            "senderNick": "Bo",
            "senderStaffId": "staff-9",
            "chatbotUserId": "bot-9",
            "chatbotCorpId": "corp-9",
            "senderCorpId": "sender-corp",
            "robotCode": "robot-9",
            "sessionWebhook": "https://oapi.dingtalk.com/hook",
            "sessionWebhookExpiredTime": 1_690_000_300_000_i64,
            "createAt": 1_690_000_000_000_i64,
            "isInAtList": true,
            "isAdmin": false,
            "msgtype": "text",
            "text": { "content": "  hi  " },
            "atUsers": [{ "dingtalkId": "$:LWCP_v1:$1", "staffId": "staff-9" }],
            "unknownField": 7,
        });
        let message = ChatbotMessage::from_dict(&body);
        assert_eq!(message.conversation_id.as_deref(), Some("cid-9"));
        assert_eq!(message.conversation_title.as_deref(), Some("Team"));
        assert!(!message.is_group());
        assert_eq!(message.message_id.as_deref(), Some("m-9"));
        assert_eq!(message.sender_id.as_deref(), Some("u-9"));
        assert_eq!(message.sender_nick.as_deref(), Some("Bo"));
        assert_eq!(message.sender_staff_id.as_deref(), Some("staff-9"));
        assert_eq!(message.chatbot_user_id.as_deref(), Some("bot-9"));
        assert_eq!(message.chatbot_corp_id.as_deref(), Some("corp-9"));
        assert_eq!(message.sender_corp_id.as_deref(), Some("sender-corp"));
        assert_eq!(message.robot_code.as_deref(), Some("robot-9"));
        assert_eq!(
            message.session_webhook_expired_time,
            Some(1_690_000_300_000)
        );
        assert_eq!(message.create_at, Some(1_690_000_000_000));
        assert_eq!(message.is_in_at_list, Some(true));
        assert_eq!(message.is_admin, Some(false));
        assert_eq!(message.at_users.len(), 1);
        assert_eq!(message.at_users[0].staff_id.as_deref(), Some("staff-9"));
        // Unknown fields land in extensions (chatbot.py:250-251).
        assert_eq!(message.extensions.get("unknownField"), Some(&json!(7)));
        assert_eq!(message.text(), "hi");

        let event = message.to_event().unwrap_or_else(empty_event);
        assert_eq!(event.message_type, MessageType::Text);
        assert_eq!(event.source.chat_id, "cid-9");
        assert_eq!(event.source.chat_name.as_deref(), Some("Team"));
        assert_eq!(event.metadata[META_CONVERSATION_TYPE], json!("1"));
        assert_eq!(event.metadata[META_IS_IN_AT_LIST], json!(true));
        assert_eq!(event.metadata[META_SENDER_STAFF_ID], json!("staff-9"));
    }

    fn empty_event() -> MessageEvent {
        MessageEvent::new("", SessionSource::new(dingtalk_platform(), ""))
    }

    #[test]
    fn chatbot_group_maps_to_a_group_chat_and_keeps_the_mention() {
        let body = json!({
            "conversationId": "cidg==",
            "conversationTitle": "Ops",
            "conversationType": "2",
            "msgId": "m-g",
            "senderId": "u-g",
            "isInAtList": true,
            "msgtype": "text",
            "text": { "content": "@vikram /status" },
        });
        let message = ChatbotMessage::from_dict(&body);
        assert!(message.is_group());
        let event = message.to_event().unwrap_or_else(empty_event);
        assert_eq!(event.source.chat_type, "group");
        assert_eq!(event.source.chat_name.as_deref(), Some("Ops"));
        // inbound.py:103-105 — the literal @handle survives; the mention is structural.
        assert_eq!(event.text, "@vikram /status");
        // A missing nick falls back to the id (adapter.py:466).
        assert_eq!(event.source.user_name.as_deref(), Some("u-g"));
        assert_eq!(event.metadata[META_IS_IN_AT_LIST], json!(true));
    }

    #[test]
    fn chatbot_richtext_joins_items_with_spaces() {
        let body = json!({
            "conversationId": "cid-1",
            "conversationType": "1",
            "msgId": "m-r",
            "msgtype": "richText",
            "content": {
                "richText": [
                    { "text": "first line" },
                    { "downloadCode": "code-1" },
                    { "text": "second line" },
                ]
            },
        });
        let message = ChatbotMessage::from_dict(&body);
        let rich = message.rich_text_content.clone().unwrap_or_default();
        assert_eq!(rich.rich_text_list.len(), 3);
        assert_eq!(message.text(), "first line second line");
        assert_eq!(
            message.to_event().map(|event| event.text),
            Some("first line second line".to_string())
        );
    }

    #[test]
    fn chatbot_handles_missing_and_odd_fields() {
        // No text and no media ⇒ nothing to route (adapter.py:484-485).
        let empty = ChatbotMessage::from_dict(&json!({ "conversationId": "cid-1" }));
        assert_eq!(empty.text(), "");
        assert!(empty.to_event().is_none());

        // Neither conversation nor sender ⇒ no chat to answer.
        let orphan = ChatbotMessage::from_dict(&json!({
            "msgtype": "text",
            "text": { "content": "hello" },
        }));
        assert!(orphan.to_event().is_none());

        // `msgtype: text` without its `text` block: the reference would raise (chatbot.py:230);
        // here the content is simply absent.
        let truncated = ChatbotMessage::from_dict(&json!({
            "conversationId": "cid-1",
            "msgtype": "text",
        }));
        assert!(truncated.text.is_none());
        assert_eq!(truncated.text(), "");

        // A sender with no conversation routes to the sender's own chat.
        let dm = ChatbotMessage::from_dict(&json!({
            "senderId": "u-1",
            "msgtype": "text",
            "text": { "content": "hey" },
        }));
        assert_eq!(dm.chat_id(), "u-1");
        assert_eq!(
            dm.to_event().map(|event| event.source.chat_id),
            Some("u-1".to_string())
        );

        // `audio` and `file` carry their text outside `text`/`content.richText`
        // (inbound.py:80-85).
        let audio = ChatbotMessage::from_dict(&json!({
            "conversationId": "cid-1",
            "msgtype": "audio",
            "content": { "recognition": " spoken words " },
        }));
        assert_eq!(audio.text(), "spoken words");
        assert_eq!(
            audio.to_event().map(|event| event.message_type),
            Some(MessageType::Voice)
        );
        let file = ChatbotMessage::from_dict(&json!({
            "conversationId": "cid-1",
            "msgtype": "file",
            "content": { "fileName": "notes.pdf" },
        }));
        assert_eq!(file.text(), "[文件] notes.pdf");
        assert_eq!(
            file.to_event().map(|event| event.message_type),
            Some(MessageType::Document)
        );

        // A picture is a Photo with no text; it is dropped rather than mislabelled.
        let picture = ChatbotMessage::from_dict(&json!({
            "conversationId": "cid-1",
            "msgtype": "picture",
            "content": { "downloadCode": "code-2" },
        }));
        assert_eq!(
            picture
                .image_content
                .as_ref()
                .and_then(|content| content.download_code.clone()),
            Some("code-2".to_string())
        );
        assert_eq!(picture.text(), "");
        assert!(picture.to_event().is_none());
    }

    #[test]
    fn non_object_bodies_parse_to_empty_messages() {
        for body in [json!(null), json!([]), json!("text"), json!(3)] {
            let message = ChatbotMessage::from_dict(&body);
            assert_eq!(message, ChatbotMessage::default());
            assert!(message.to_event().is_none());
        }
    }

    // ----- reconnect policy ----------------------------------------------------------------

    #[test]
    fn close_outcomes_carry_the_library_backoff() {
        // stream.py:69 / 85 / 88 and the immediate reopen after a clean close.
        assert_eq!(CloseOutcome::Clean.backoff(), Duration::ZERO);
        assert_eq!(CloseOutcome::Network.backoff(), Duration::from_secs(10));
        assert_eq!(CloseOutcome::OpenFailed.backoff(), Duration::from_secs(10));
        assert_eq!(CloseOutcome::Unknown.backoff(), Duration::from_secs(3));
        assert_eq!(OPEN_CONNECTION_RETRY_SECS, 10);
        assert_eq!(NETWORK_RETRY_SECS, 10);
        assert_eq!(UNKNOWN_RETRY_SECS, 3);
        assert_eq!(START_FOREVER_RETRY_SECS, 3);

        // websockets' ConnectionClosedOK (1000, 1001) versus ConnectionClosedError.
        for code in [1000u16, 1001] {
            assert_eq!(close_outcome_for_code(code), CloseOutcome::Clean, "{code}");
        }
        for code in [1005u16, 1006, 1011, 1012, 4000, 4999] {
            assert_eq!(
                close_outcome_for_code(code),
                CloseOutcome::Network,
                "{code}"
            );
        }
    }

    #[test]
    fn socket_results_map_onto_the_policy() {
        assert_eq!(
            CloseOutcome::from_socket_result(Ok(None)),
            CloseOutcome::Clean
        );
        assert_eq!(
            CloseOutcome::from_socket_result(Err(SocketError::Closed)),
            CloseOutcome::Network
        );
        assert_eq!(
            CloseOutcome::from_socket_result(Err(SocketError::Io("reset".to_string()))),
            CloseOutcome::Network
        );
        // A frame is never a close; the placeholder keeps the mapping total.
        assert_eq!(
            CloseOutcome::from_socket_result(Ok(Some("{}".to_string()))),
            CloseOutcome::Unknown
        );
    }

    #[tokio::test]
    async fn a_closed_socket_reopens_and_keeps_reading() {
        let first = MockSocket::new(vec![Ok(None)]);
        let second = MockSocket::new(vec![Ok(Some(text_callback("m2", "after reopen")))]);
        let mock = MockTransport::new(vec![
            open_ok("wss://host/one", "t-1"),
            open_ok("wss://host/two", "t-2"),
        ]);
        let (client, urls) = client(&mock, vec![first, Arc::clone(&second)]);
        assert!(client.open_connection().await.is_ok());

        let event = client.next_event().await.unwrap_or(None);
        assert_eq!(
            event.map(|event| event.text),
            Some("after reopen".to_string())
        );
        assert_eq!(lock(&urls).len(), 2);
        assert_eq!(mock.requests().len(), 2);
    }

    #[tokio::test]
    async fn a_failed_reopen_is_reported() {
        // A clean close reopens with no wait; the `Err` branch adds the 10 s network wait asserted
        // in `close_outcomes_carry_the_library_backoff`, so it is not exercised here.
        let first = MockSocket::new(vec![Ok(None)]);
        // One scripted open: the first open consumes it, so the reopen runs out and fails.
        let mock = MockTransport::new(vec![open_ok("wss://host/one", "t-1")]);
        let (client, _) = client(&mock, vec![first]);
        assert!(client.open_connection().await.is_ok());

        let message = match client.next_event().await {
            Err(DingTalkStreamError::Transport(message)) => message,
            Err(other) => format!("wrong error: {other}"),
            Ok(_) => "no error at all".to_string(),
        };
        assert!(
            message.contains("ran out of scripted responses"),
            "a failed reopen must not look like a quiet stream: {message}"
        );
        // The dead socket was dropped, so a later call reports the disconnected state instead of
        // reading from a socket that cannot work.
        assert!(!client.is_connected());
        assert!(matches!(
            client.next_event().await,
            Err(DingTalkStreamError::NotConnected)
        ));
    }

    #[tokio::test]
    async fn an_unconnected_client_says_so() {
        let mock = MockTransport::new(Vec::new());
        let (client, _) = client(&mock, Vec::new());
        assert!(matches!(
            client.next_event().await,
            Err(DingTalkStreamError::NotConnected)
        ));
        assert!(mock.requests().is_empty());
    }

    // ----- webhook replies -----------------------------------------------------------------

    #[tokio::test]
    async fn reply_posts_the_reference_markdown_body() {
        let mock = MockTransport::new(vec![http(200, "{}")]);
        let (client, _) = client(&mock, Vec::new());
        let result = client
            .reply(
                "https://oapi.dingtalk.com/robot/sendBySession?session=abc",
                "hello",
            )
            .await;
        assert!(result.success);
        assert_eq!(result.error, None);
        // `uuid.uuid4().hex[:12]` (adapter.py:535): twelve lowercase hex characters.
        let id = result.message_id.unwrap_or_default();
        assert_eq!(id.len(), 12, "{id}");
        assert!(id.chars().all(|ch| ch.is_ascii_hexdigit()), "{id}");

        let requests = mock.requests();
        assert_eq!(requests.len(), 1);
        assert_eq!(requests[0].method, HttpMethod::Post);
        assert_eq!(
            requests[0].url,
            "https://oapi.dingtalk.com/robot/sendBySession?session=abc"
        );
        assert!(
            requests[0]
                .headers
                .contains(&("Content-Type".to_string(), "application/json".to_string()))
        );
        assert_eq!(
            mock.bodies()[0],
            json!({
                "msgtype": "markdown",
                "markdown": { "title": "VIKRAM", "text": "hello" },
            })
        );
    }

    #[tokio::test]
    async fn reply_truncates_at_the_adapter_cap() {
        let mock = MockTransport::new(vec![http(200, "{}")]);
        let (client, _) = client(&mock, Vec::new());
        let long = "é".repeat(MAX_REPLY_CHARS + 50);
        assert!(
            client
                .reply("https://oapi.dingtalk.com/hook", &long)
                .await
                .success
        );
        let body = mock.bodies().remove(0);
        let text = body["markdown"]["text"].as_str().unwrap_or_default();
        // adapter.py:529 truncates by characters, not bytes.
        assert_eq!(text.chars().count(), MAX_REPLY_CHARS);
    }

    #[tokio::test]
    async fn reply_reports_non_2xx_truthfully() {
        for (status, kind, retryable) in [
            (400u16, "unknown", false),
            (401, "forbidden", false),
            (403, "forbidden", false),
            (404, "not_found", false),
            (429, "rate_limited", true),
            (500, "transient", true),
            (503, "transient", true),
        ] {
            let mock = MockTransport::new(vec![http(status, "nope")]);
            let (client, _) = client(&mock, Vec::new());
            let result = client.reply("https://oapi.dingtalk.com/hook", "hi").await;
            assert!(!result.success, "{status}");
            assert_eq!(result.error_kind.as_deref(), Some(kind), "{status}");
            assert_eq!(result.retryable, retryable, "{status}");
            assert!(
                result
                    .error
                    .as_deref()
                    .unwrap_or_default()
                    .contains(&status.to_string()),
                "{status}: {:?}",
                result.error
            );
            assert!(result.message_id.is_none(), "{status}");
        }

        // A request that never landed is transient.
        let down = MockTransport::new(vec![Err(TransportError::Request(
            "connection reset".to_string(),
        ))]);
        let (client, _) = client(&down, Vec::new());
        let result = client.reply("https://oapi.dingtalk.com/hook", "hi").await;
        assert!(!result.success);
        assert_eq!(result.error_kind.as_deref(), Some("transient"));
        assert!(result.retryable);
    }

    #[tokio::test]
    async fn reply_refuses_a_missing_webhook_without_a_request() {
        let mock = MockTransport::new(Vec::new());
        let (client, _) = client(&mock, Vec::new());
        for webhook in ["", "   "] {
            let result = client.reply(webhook, "hi").await;
            assert!(!result.success);
            assert_eq!(result.error_kind.as_deref(), Some("bad_format"));
        }
        assert!(mock.requests().is_empty());
    }

    #[tokio::test]
    async fn send_to_webhook_posts_a_caller_supplied_body() {
        let mock = MockTransport::new(vec![http(200, "{}")]);
        let (client, _) = client(&mock, Vec::new());
        // The library's `reply_text` body (chatbot.py:625-633).
        let body = json!({
            "msgtype": "text",
            "text": { "content": "plain" },
            "at": { "atUserIds": ["staff-1"] },
        });
        assert!(
            client
                .send_to_webhook("https://oapi.dingtalk.com/hook", &body)
                .await
                .success
        );
        assert_eq!(mock.bodies()[0], body);
    }

    #[test]
    fn normalize_markdown_matches_the_adapter() {
        // adapter.py:700-708 — a blank line before a numbered list that follows text.
        assert_eq!(
            normalize_markdown("Steps:\n1. one\n2. two"),
            "Steps:\n\n1. one\n2. two"
        );
        // A list that already starts the block is untouched.
        assert_eq!(normalize_markdown("1. one\n2. two"), "1. one\n2. two");
        // Fences are dedented — both the opener and its closer; other indentation is preserved
        // (adapter.py:707).
        assert_eq!(
            normalize_markdown("text\n    ```rust\n    code\n    ```"),
            "text\n```rust\n    code\n```"
        );
        assert_eq!(normalize_markdown(""), "");
        assert!(is_numbered("12. yes"));
        assert!(!is_numbered("12.no"));
        assert!(!is_numbered("no"));
        assert!(!is_numbered("."));
        assert!(!is_numbered("1."));
    }

    // ----- misc ----------------------------------------------------------------------------

    #[test]
    fn local_ip_detection_is_probabilistic_but_never_fabricated() {
        let ip = detect_local_ip();
        // Either a real local address or "" — never an invented one (stream.py:188-195).
        assert!(
            ip.is_empty() || ip.parse::<std::net::IpAddr>().is_ok(),
            "{ip}"
        );
    }

    #[test]
    fn dedup_is_bounded_and_ignores_empty_ids() {
        let mut dedup = MessageDeduplicator::new(2);
        assert!(!dedup.is_duplicate(""));
        assert!(!dedup.is_duplicate("a"));
        assert!(dedup.is_duplicate("a"));
        assert!(!dedup.is_duplicate("b"));
        assert!(!dedup.is_duplicate("c"));
        // "a" was evicted by the third id, so it is dispatutable again.
        assert!(!dedup.is_duplicate("a"));
        assert!(dedup.is_duplicate("c"));
    }

    #[test]
    fn subscriptions_serialize_as_the_library_sends_them() {
        assert_eq!(
            Subscription::callback(CHATBOT_TOPIC).to_value(),
            json!({ "type": "CALLBACK", "topic": "/v1.0/im/bot/messages/get" })
        );
        assert_eq!(
            Subscription::events_all().to_value(),
            json!({ "type": "EVENT", "topic": "*" })
        );
        assert_eq!(SubscriptionKind::Event.as_str(), "EVENT");
        assert_eq!(SubscriptionKind::Callback.as_str(), "CALLBACK");
    }

    #[test]
    fn ack_data_is_json_encoded_into_a_string() {
        let frame = Frame::from_value(&json!({
            "type": "CALLBACK",
            "headers": { "messageId": "m" },
            "data": "{}",
        }));
        let ack = AckMessage::for_callback(&frame, ACK_CODE_OK, "OK");
        let value = ack.to_value();
        assert_eq!(value["data"], json!("{\"response\":\"OK\"}"));
        assert_eq!(value["headers"]["messageId"], json!("m"));
        assert_eq!(value["headers"]["contentType"], json!("application/json"));
        assert_eq!(value["message"], json!(""));
        assert_eq!(value["code"], json!(200));
    }

    #[test]
    fn frame_parsing_keeps_extensions_and_parses_data() {
        let frame = Frame::from_value(&json!({
            "specVersion": "1.0",
            "type": "CALLBACK",
            "headers": { "messageId": "m", "time": 1690000000000_i64 },
            "data": "{\"a\":1}",
            "extra": true,
        }));
        assert_eq!(frame.kind, "CALLBACK");
        assert_eq!(frame.spec_version.as_deref(), Some("1.0"));
        assert_eq!(frame.headers.time, Some(1_690_000_000_000));
        assert_eq!(frame.data, json!({ "a": 1 }));
        assert_eq!(frame.extensions.get("extra"), Some(&json!(true)));
        // `time` is stringified in the echo (frames.py:53-54).
        assert_eq!(frame.headers.to_value()["time"], json!("1690000000000"));

        // Unparsable `data` leaves Null rather than failing the frame.
        let broken = Frame::from_value(&json!({ "type": "CALLBACK", "data": "not json" }));
        assert_eq!(broken.data, Value::Null);
        assert!(Frame::parse("nope").is_none());
        assert!(Frame::parse("[]").is_none());
    }

    #[test]
    fn a_parsed_callback_body_yields_an_event_and_its_ack() {
        let mock = MockTransport::new(Vec::new());
        let (client, _) = client(&mock, Vec::new());
        let body = json!({
            "conversationId": "cid-2",
            "conversationType": "2",
            "msgId": "m-2",
            "senderId": "u-2",
            "msgtype": "text",
            "text": { "content": "hi" },
        });
        let (event, ack) = match client.parse_callback_body(&body) {
            Ok(pair) => pair,
            Err(ack) => (empty_event(), *ack),
        };
        assert_eq!(event.text, "hi");
        assert_eq!(event.source.chat_type, "group");
        assert_eq!(ack.code, ACK_CODE_OK);
        assert_eq!(ack.headers.message_id.as_deref(), Some("m-2"));
        // The same body twice: the second is reported as a duplicate (still ACKed OK).
        assert!(client.parse_callback_body(&body).is_err());
        // A non-object body is a 500.
        let refused = client.parse_callback_body(&json!("text"));
        assert!(matches!(refused, Err(ack) if ack.code == ACK_CODE_SYSTEM_EXCEPTION));
    }

    #[test]
    fn the_stream_wrapper_exposes_the_host_surface() {
        let mock = MockTransport::new(Vec::new());
        let stream = DingTalkStream::new(
            Credential::new(CLIENT_ID, CLIENT_SECRET),
            Arc::clone(&mock) as SharedTransport,
        )
        .with_local_ip(None)
        .with_open_url("https://mirror.test/");
        assert_eq!(
            stream.subscriptions(),
            &[Subscription::callback(CHATBOT_TOPIC)]
        );
        assert!(!stream.is_connected());
        assert_eq!(
            stream.client().open_connection_request().url,
            "https://mirror.test"
        );
        // `local_ip: None` sends "" like a failed get_host_ip.
        let body = body_of(&stream.client().open_connection_request());
        assert_eq!(body["localIp"], json!(""));
        stream.close();
        assert!(mock.requests().is_empty());
    }

    #[test]
    fn message_ids_are_twelve_hex_characters() {
        let id = new_message_id();
        assert_eq!(id.len(), 12);
        assert!(id.chars().all(|ch| ch.is_ascii_hexdigit()));
        assert_ne!(id, new_message_id());
    }
}
