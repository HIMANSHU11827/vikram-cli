//! Raft adapter — a wake-only local bridge.
//!
//! Rust port of hermes-agent `plugins/platforms/raft/adapter.py` (the `raft-platform` plugin, see
//! `plugin.yaml` v1.0.0). The shape is unusual and worth stating up front: **nothing here carries
//! message content**. The `raft` CLI's bridge (`raft agent bridge --wake-adapter wake-channel`)
//! owns the Raft cursor and the message bodies; all it hands the agent is a content-free wake
//! hint, and the agent then reads whatever is pending with the raft CLI itself. The adapter's whole
//! job is to turn that hint into one [`MessageEvent`] per wake and let the host run a turn.
//!
//! Ported behaviour, with the reference origin:
//! - Endpoint surface (`connect`, adapter.py:331-335): `GET /health`, `POST /wake` (path
//!   configurable), `POST /activity`, `GET /activity/drain`. The routes are served through this
//!   crate's existing ingress seam — [`RaftAdapter`] implements [`WebhookHandler`], so a host
//!   mounts [`RaftAdapter::routes`] on [`crate::ingress::WebhookHub`] and runs
//!   [`crate::ingress::serve`]. hermes binds its own aiohttp site and spawns the bridge inside
//!   `connect`; both are the host's job here (see "not possible" below).
//! - Auth: every route but `/health` requires the `x-raft-bridge-token` header, compared in
//!   constant time (`_authorized`, adapter.py:424-427). A missing or wrong token is
//!   `401 {"ok": false, "error": "unauthorized"}`. `/health` is unauthenticated in the reference
//!   (`_handle_health`, adapter.py:419-422) and stays so.
//! - Body cap: `max_body_bytes` (default 16 384) is enforced on the declared `Content-Length` and
//!   again on the bytes actually read → `413 {"ok": false, "error": "payload_too_large"}`
//!   (`_read_bridge_body`, adapter.py:429-446; the reference's `client_max_size` is the ingress
//!   server's job here).
//! - Wake parsing (`_handle_wake`, adapter.py:448-477): a blank body is `{}`; invalid JSON is
//!   `400 invalid_json`; a non-object is `400 invalid_payload`; any key named `body`, `content`,
//!   `message`, `messages`, `preview`, `snippet` or `text` — at any depth — is
//!   `400 content_not_allowed` (`_has_content_field`, adapter.py:95-98). There is deliberately no
//!   `schema` gate: the bridge owns schema evolution and the adapter only proves the payload is
//!   content-free (adapter.py:458).
//! - The wake event is the SAME for every accepted payload: `text` is the fixed [`WAKE_PROMPT`]
//!   (adapter.py:58-61), the type is text, `internal` is `true`, `raw_message` is the parsed
//!   payload, and `message_id` is the first present non-empty value among `eventId`, `attemptId`,
//!   `messageId`, `delivery_id`, `wake_id`, `id`, else the synthetic `raft-wake-{unix-millis}`
//!   (`_WAKE_ID_KEYS`, adapter.py:57 and 465-467). The source is chat `runtime_session` (default
//!   `default`), chat name `Raft channel`, chat type `dm`, user `raft-bridge` / `Raft Bridge`
//!   (adapter.py:468-471).
//! - A wake that arrives before a message handler is attached is
//!   `503 {"ok": false, "error": "not_ready", "runtimeSession": …}` (adapter.py:461-464); an
//!   accepted wake is `202 {"ok": true, "runtimeSession": …}`.
//! - Activity telemetry (`POST /activity`, `GET /activity/drain`) with the `raft-activity.v1`
//!   validation, its allowed-field list, the 4 KiB content cap with its truncation flags, the
//!   bounded queue (default 500) that counts drops since the last drain, and the
//!   `raft-activity-drain.v1` drain envelope (adapter.py:54-199 and 479-495).
//! - `get_chat_info` answers `{"name": "raft/<chat>", "type": "raft"}` (adapter.py:416-417) —
//!   note `raft`, not the `dm` the wake source carries; both are kept exactly as they are.
//! - Wake de-duplication is made explicit. The reference adapter does not dedupe: every accepted
//!   wake goes to `super().handle_message(event)` (adapter.py:499-502), and the base
//!   `handle_message` is the stage that drops a repeat of a message id — that base class is not
//!   part of the reference this port was written from. Rather than leave the stage undefined, this
//!   port implements it directly: an id that already woke the gateway inside
//!   [`DEFAULT_WAKE_DEDUP_WINDOW_MS`] (5 minutes, configurable per adapter) does not open a second
//!   turn, while the bridge still receives the `202` the reference would answer. A wake carrying
//!   none of `_WAKE_ID_KEYS` is never deduplicated: its synthetic millisecond id is not the
//!   bridge's own delivery identity, so suppressing it could drop a real wake. The reference
//!   enforces no cooldown and no rate limit — a burst of distinct wakes is a burst of turns — so
//!   none is invented here.
//!
//! Deliberate deviations, every one of them a refusal to fake a capability:
//! - `send` FAILS. The reference returns `SendResult(success=True)` for a no-op (adapter.py:
//!   411-414) while nothing is delivered — the agent posts messages through the raft CLI. A
//!   success here would be a phantom delivery in the ledger, so [`RaftAdapter::send`] returns
//!   [`SendResult::failed`] with [`WAKE_ONLY_SEND_REFUSAL`]. Raft is not a sending platform; a host
//!   that mounts it as one must treat that failure as expected.
//! - The busy-session coalescing in `handle_message` (`merge_pending_message_event`, adapter.py:
//!   504-509) needs the base class's live-session set, which belongs to the host: every accepted
//!   wake is handed to the attached handler and the host decides whether to queue it.
//! - hermes wraps the dispatch in `try/except` and answers `503 not_ready` when it raises
//!   (adapter.py:472-476). A Rust handler cannot raise, and a panic is not something this adapter
//!   catches.
//! - The plugin layer: `register` / `ctx.register_hook` and the session, tool and turn hooks
//!   (adapter.py:241-301 and 560-587), `check_raft_requirements`, `_is_connected`,
//!   `_env_enablement`, `interactive_setup` and the `RAFT_PROFILE` secret-scope resolution. Those
//!   are host plugin plumbing, not wire behaviour; [`RaftAdapter::report_activity`] is kept so a
//!   host hook can still feed the queue exactly as `_emit` did.
//! - Process supervision: the reference spawns the bridge as an owned child and `terminate()` /
//!   `kill()`s it on `disconnect` (`_spawn_bridge` / `_stop_bridge`, adapter.py:370-409). Here
//!   [`RaftAdapter::bridge_spec`] hands over the exact argv and environment the reference builds,
//!   and the host owns the child however it supervises processes.
//!
//! Not possible without the raft CLI, and why:
//! - **No message content, ever.** The adapter never sees a Raft message body or a delivery
//!   cursor: the bridge owns both, and the agent reads them with `raft` commands. Anything this
//!   adapter printed as "message text" would be invented, so the wake text is a fixed instruction
//!   to check Raft ([`WAKE_PROMPT`]) and nothing else from the payload is ever surfaced.
//! - **No wake source.** Without `raft` on `PATH` and a `RAFT_PROFILE`, nothing POSTs to `/wake`:
//!   the reference degrades to what it calls "wake-only polling mode" with the bridge unspawned
//!   (adapter.py:371-376). The endpoint stays up and answers, but it will never be called.
//! - **No reply path.** The agent's answer leaves through the raft CLI, not through here — hence
//!   the failed `send` above, and hence [`WAKE_ONLY_SEND_REFUSAL`] as the operator-facing reason.
//! - **No listening socket inside `connect`.** hermes binds `127.0.0.1:0` and reports the bound
//!   port; this crate's inbound server is [`crate::ingress`], which a host runs. `connect`
//!   therefore materialises the token, refuses a fixed port that is already taken (the reference's
//!   own probe, adapter.py:336-344) and reports readiness; [`RaftAdapter::routes`] and
//!   [`RaftAdapter::endpoint`] give the host what it needs to serve and to spawn the bridge.

use std::collections::BTreeSet;
use std::collections::VecDeque;
use std::fmt::Write as _;
use std::net::{SocketAddr, TcpStream};
use std::sync::{Arc, Mutex, MutexGuard};
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use async_trait::async_trait;
use serde_json::{Map, Value, json};

use crate::event::{MessageEvent, MessageType, Platform, SessionSource};
use crate::ingress::WebhookHandler;
use crate::ingress::WebhookRequest;
use crate::platform::{ChatInfo, MessageHandler, PlatformAdapter, SendResult};

/// Loopback host the reference binds (adapter.py:47 `DEFAULT_HOST`).
pub const DEFAULT_HOST: &str = "127.0.0.1";

/// `DEFAULT_PORT`: 0 means "let the OS pick" — and, in the reference, skips the port probe.
pub const DEFAULT_PORT: u16 = 0;

/// `DEFAULT_PATH`: the route the bridge POSTs wakes to.
pub const DEFAULT_PATH: &str = "/wake";

/// `DEFAULT_RUNTIME_SESSION`: the chat id a wake lands in when none is configured.
pub const DEFAULT_RUNTIME_SESSION: &str = "default";

/// `DEFAULT_MAX_BODY_BYTES`: the wake/activity body cap, in bytes.
pub const DEFAULT_MAX_BODY_BYTES: usize = 16_384;

/// `DEFAULT_ACTIVITY_QUEUE_CAP`: how many activity events the queue retains.
pub const DEFAULT_ACTIVITY_QUEUE_CAP: usize = 500;

/// `ACTIVITY_CONTENT_CAP`: characters of `toolInput` / `toolOutput` kept before truncation.
pub const ACTIVITY_CONTENT_CAP: usize = 4096;

/// `ACTIVITY_EVENT_SCHEMA`: the only activity schema the queue accepts.
pub const ACTIVITY_EVENT_SCHEMA: &str = "raft-activity.v1";

/// `ACTIVITY_DRAIN_SCHEMA`: the envelope `GET /activity/drain` answers with.
pub const ACTIVITY_DRAIN_SCHEMA: &str = "raft-activity-drain.v1";

/// `BRIDGE_TOKEN_HEADER` (adapter.py:56): the header carrying the bridge's shared secret.
pub const BRIDGE_TOKEN_HEADER: &str = "x-raft-bridge-token";

/// `_WAKE_ID_KEYS` (adapter.py:57): where a bridge may put the delivery identity, in precedence
/// order. The first key present with a non-empty value becomes the event's `message_id`.
pub const WAKE_ID_KEYS: [&str; 6] = [
    "eventId",
    "attemptId",
    "messageId",
    "delivery_id",
    "wake_id",
    "id",
];

/// `_WAKE_PROMPT` (adapter.py:58-61): the ONE text every wake carries.
///
/// Content-free by construction: it tells the agent to check Raft and how to learn the CLI, and it
/// is byte-identical for a wake, a notify and a heartbeat payload. Nothing in the payload ever
/// reaches it — that is the point of the design, not an omission.
pub const WAKE_PROMPT: &str = concat!(
    "Raft wake hint received. New Raft messages may be pending. ",
    "If you have not read the Raft manual in this session, run ",
    "`raft manual get raft-cli-overview` before using Raft commands."
);

/// `_CONTENT_FIELD_NAMES` (adapter.py:63): a payload key, at any depth, that means the bridge sent
/// message content and the wake must be refused.
pub const CONTENT_FIELD_NAMES: [&str; 7] = [
    "body", "content", "message", "messages", "preview", "snippet", "text",
];

/// `_MAX_SCALAR_LENGTH` (adapter.py:65): the longest scalar an activity event may carry.
pub const MAX_SCALAR_LENGTH: usize = 120;

/// `_ACTIVITY_ALLOWED_FIELDS` (adapter.py:66-68): every field an activity event may carry.
pub const ACTIVITY_ALLOWED_FIELDS: [&str; 14] = [
    "schema",
    "eventId",
    "sessionId",
    "hookEventName",
    "status",
    "occurredAt",
    "toolName",
    "toolInput",
    "toolOutput",
    "toolInputTruncated",
    "toolOutputTruncated",
    "truncated",
    "errorClass",
    "durationMs",
];

/// Default wake de-duplication window. The reference delegates dedup to its base
/// `handle_message` stage (adapter.py:499-502); this port owns that stage and pins the window here
/// so the behaviour is visible and configurable instead of implied.
pub const DEFAULT_WAKE_DEDUP_WINDOW_MS: u64 = 300_000;

/// Default number of delivery ids remembered for de-duplication.
pub const DEFAULT_WAKE_DEDUP_CAP: usize = 512;

/// `_handle_activity_drain`'s default page size (adapter.py:494).
pub const DEFAULT_DRAIN_MAX: usize = 200;

/// The reason [`RaftAdapter::send`] always fails. Raft has no outbound message API in this
/// adapter's contract: the agent posts messages with the raft CLI.
pub const WAKE_ONLY_SEND_REFUSAL: &str = "raft is a wake-only bridge: this adapter has no outbound \
message API, so nothing was sent — the agent posts messages through the raft CLI";

/// `Platform("raft")` (adapter.py:311). `event::platform_names` predates this plugin and is
/// frozen, so the name lives here.
const PLATFORM_NAME: &str = "raft";

/// `chat_name="Raft channel"` on every wake source (adapter.py:468).
const WAKE_CHAT_NAME: &str = "Raft channel";

/// `chat_type="dm"` on every wake source (adapter.py:468).
const WAKE_CHAT_TYPE: &str = "dm";

/// `user_id="raft-bridge"` on every wake source (adapter.py:469).
const WAKE_USER_ID: &str = "raft-bridge";

/// `user_name="Raft Bridge"` on every wake source (adapter.py:469).
const WAKE_USER_NAME: &str = "Raft Bridge";

/// The `type` `get_chat_info` reports — `"raft"`, not `dm` (adapter.py:417).
const CHAT_INFO_TYPE: &str = "raft";

/// Route: liveness, unauthenticated, with the activity queue's size (adapter.py:332, 419-422).
pub const HEALTH_PATH: &str = "/health";

/// Route: activity push (adapter.py:334).
pub const ACTIVITY_PATH: &str = "/activity";

/// Route: activity drain (adapter.py:335).
pub const ACTIVITY_DRAIN_PATH: &str = "/activity/drain";

/// A wall-clock source in unix milliseconds. Injected so tests can move time and so the adapter
/// owns no global state.
pub type Clock = Arc<dyn Fn() -> u64 + Send + Sync>;

/// The real clock: unix milliseconds, saturating at the epoch.
pub fn system_clock() -> Clock {
    Arc::new(|| {
        SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .map(|elapsed| elapsed.as_millis() as u64)
            .unwrap_or(0)
    })
}

/// Everything `platforms.raft.extra` configures in the reference (adapter.py:310-323).
///
/// `Default` is the reference's default configuration: loopback, an OS-chosen port, `/wake`, no
/// bridge token (one is generated on `connect`), the `default` runtime session and a 16 KiB body
/// cap.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RaftConfig {
    /// Interface the wake endpoint is served on (`127.0.0.1`).
    pub host: String,
    /// Fixed port, or 0 for "the host picks one".
    pub port: u16,
    /// Wake route; normalised to a leading `/`, blank falls back to [`DEFAULT_PATH`].
    pub path: String,
    /// The bridge's shared secret. `None`/blank means "generate one on connect".
    pub bridge_token: Option<String>,
    /// Chat id wakes land in (`default`).
    pub runtime_session: String,
    /// Body cap for every route, in bytes.
    pub max_body_bytes: usize,
    /// How long a delivery id is remembered for de-duplication.
    pub dedup_window_ms: u64,
    /// How many delivery ids are remembered at most.
    pub dedup_cap: usize,
    /// How many activity events the queue retains.
    pub activity_queue_cap: usize,
}

impl Default for RaftConfig {
    fn default() -> Self {
        Self {
            host: DEFAULT_HOST.to_string(),
            port: DEFAULT_PORT,
            path: DEFAULT_PATH.to_string(),
            bridge_token: None,
            runtime_session: DEFAULT_RUNTIME_SESSION.to_string(),
            max_body_bytes: DEFAULT_MAX_BODY_BYTES,
            dedup_window_ms: DEFAULT_WAKE_DEDUP_WINDOW_MS,
            dedup_cap: DEFAULT_WAKE_DEDUP_CAP,
            activity_queue_cap: DEFAULT_ACTIVITY_QUEUE_CAP,
        }
    }
}

/// The `raft agent bridge` invocation `connect` spawns in the reference (adapter.py:377-382).
///
/// Built, never spawned: process ownership (kill-the-child-if-the-gateway-dies) is host
/// supervision, not wire behaviour. The argv and environment are exactly the reference's, so a
/// host can spawn it verbatim.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct BridgeSpec {
    /// The resolved `raft` executable (the host supplies the `PATH` lookup).
    pub program: String,
    /// Arguments after the program name, in the reference's order.
    pub args: Vec<String>,
    /// Environment variables the bridge must see; everything else is the host's call.
    pub env: Vec<(String, String)>,
}

impl BridgeSpec {
    /// `--wake-channel-endpoint` value: `http://{host}:{port}{path}` (adapter.py:377). `port` must
    /// be the port the host actually bound, which is why it is a parameter and not a field.
    pub fn endpoint(host: &str, port: u16, path: &str) -> String {
        format!("http://{host}:{port}{path}")
    }

    /// The reference's command (adapter.py:377-382), or `None` when `profile` is blank — the
    /// reference refuses to spawn without `RAFT_PROFILE` (adapter.py:374-376).
    ///
    /// The reference additionally sets `HOME` from its own `HERMES_REAL_HOME` bookkeeping; that is
    /// hermes process plumbing and is left to the host, while `RAFT_PROFILE` and
    /// `RAFT_CHANNEL_TOKEN` — the two variables the CLI actually needs — are carried here.
    pub fn new(
        program: impl Into<String>,
        profile: &str,
        host: &str,
        port: u16,
        path: &str,
        bridge_token: &str,
    ) -> Option<Self> {
        let profile = profile.trim();
        if profile.is_empty() {
            return None;
        }
        let endpoint = Self::endpoint(host, port, path);
        Some(Self {
            program: program.into(),
            args: vec![
                "--profile".to_string(),
                profile.to_string(),
                "agent".to_string(),
                "bridge".to_string(),
                "--wake-adapter".to_string(),
                "wake-channel".to_string(),
                "--wake-channel-endpoint".to_string(),
                endpoint,
            ],
            env: vec![
                ("RAFT_PROFILE".to_string(), profile.to_string()),
                ("RAFT_CHANNEL_TOKEN".to_string(), bridge_token.to_string()),
            ],
        })
    }
}

/// A rejected activity event, carrying the message the reference puts in its 400 body.
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
pub enum ActivityError {
    /// `activity event must be an object` (adapter.py:150).
    #[error("activity event must be an object")]
    NotAnObject,
    /// `unsupported activity event schema` (adapter.py:152).
    #[error("unsupported activity event schema")]
    UnsupportedSchema,
    /// The alphabetically first field outside [`ACTIVITY_ALLOWED_FIELDS`] (adapter.py:153-154).
    #[error("activity event field {0} is not allowed")]
    UnknownField(String),
    /// A required field that is not a safe non-empty scalar (adapter.py:155-157).
    #[error("activity event {0} must be a safe non-empty string")]
    UnsafeScalar(&'static str),
    /// `activity event status must be ok|error` (adapter.py:158-159).
    #[error("activity event status must be ok|error")]
    BadStatus,
    /// An optional field of the wrong type (adapter.py:160-163).
    #[error("activity event {field} must be {expected}")]
    BadField {
        /// The offending field.
        field: &'static str,
        /// What the reference says it must be.
        expected: &'static str,
    },
    /// `toolInput` / `toolOutput` that is not a string (adapter.py:169-170).
    #[error("activity event {0} must be a string")]
    NotText(&'static str),
}

/// Bounded at-most-once queue for Raft external activity telemetry (adapter.py:177-204).
///
/// Events are validated before they enter, the oldest is dropped (and counted) once `cap` is
/// reached, and a drain hands back what is queued together with the number dropped since the
/// previous drain. `Send + Sync`: the queue is shared by every HTTP worker the host runs.
#[derive(Debug)]
pub struct ActivityQueue {
    cap: usize,
    state: Mutex<ActivityState>,
}

#[derive(Debug)]
struct ActivityState {
    events: VecDeque<Value>,
    dropped_since_drain: usize,
}

impl ActivityQueue {
    /// A queue holding at most `cap` events; `0` selects [`DEFAULT_ACTIVITY_QUEUE_CAP`] (the
    /// reference's `max(1, int(cap or DEFAULT_ACTIVITY_QUEUE_CAP))`).
    pub fn new(cap: usize) -> Self {
        Self {
            cap: if cap == 0 {
                DEFAULT_ACTIVITY_QUEUE_CAP
            } else {
                cap
            },
            state: Mutex::new(ActivityState {
                events: VecDeque::new(),
                dropped_since_drain: 0,
            }),
        }
    }

    /// Validate and enqueue one event, dropping the oldest when the queue is full.
    pub fn push(&self, event: &Value) -> Result<(), ActivityError> {
        let validated = validate_activity_event(event)?;
        let mut state = lock(&self.state);
        state.events.push_back(validated);
        while state.events.len() > self.cap {
            state.events.pop_front();
            state.dropped_since_drain += 1;
        }
        Ok(())
    }

    /// Take up to `max_events` events, oldest first, and reset the drop counter.
    ///
    /// `0` selects [`DEFAULT_DRAIN_MAX`], matching `max(1, int(max_events or 200))`.
    pub fn drain(&self, max_events: usize) -> Value {
        let limit = if max_events == 0 {
            DEFAULT_DRAIN_MAX
        } else {
            max_events
        };
        let mut state = lock(&self.state);
        let take = limit.min(state.events.len());
        let events: Vec<Value> = state.events.drain(..take).collect();
        let dropped = state.dropped_since_drain;
        state.dropped_since_drain = 0;
        json!({
            "schema": ACTIVITY_DRAIN_SCHEMA,
            "events": events,
            "dropped": dropped,
        })
    }

    /// How many events are queued right now (`/health` reports this).
    pub fn size(&self) -> usize {
        lock(&self.state).events.len()
    }
}

impl Default for ActivityQueue {
    fn default() -> Self {
        Self::new(DEFAULT_ACTIVITY_QUEUE_CAP)
    }
}

/// The Raft adapter: a content-free wake hint in, one gateway turn out.
///
/// Every route is a method on this type ([`WebhookHandler`]), so a host serves the adapter by
/// mounting [`RaftAdapter::routes`] on [`crate::ingress::WebhookHub`]. The adapter holds no
/// connection, so it is `Sync` and shareable as an `Arc`; the only mutable state is the wake dedup
/// cache and the activity queue, both behind a mutex.
pub struct RaftAdapter {
    host: String,
    port: u16,
    path: String,
    bridge_token: Option<String>,
    runtime_session: String,
    max_body_bytes: usize,
    dedup_window_ms: u64,
    dedup_cap: usize,
    activity: ActivityQueue,
    /// `(unix-millis, delivery id)` of recent wakes, oldest first.
    wake_ids: Mutex<VecDeque<(u64, String)>>,
    handler: Option<MessageHandler>,
    connected: bool,
    clock: Clock,
}

impl RaftAdapter {
    /// Build an adapter from `config` on the system clock.
    pub fn new(config: RaftConfig) -> Self {
        Self::with_clock(config, system_clock())
    }

    /// Build an adapter from `config` on an injected `clock` (unix milliseconds).
    ///
    /// Configuration is normalised the way the reference does it (adapter.py:312-320): a blank
    /// host falls back to the loopback, the wake path always starts with `/`, a blank path becomes
    /// [`DEFAULT_PATH`], a blank runtime session becomes [`DEFAULT_RUNTIME_SESSION`], and a
    /// whitespace-only bridge token counts as unset so `connect` generates one.
    pub fn with_clock(config: RaftConfig, clock: Clock) -> Self {
        let host = {
            let trimmed = config.host.trim();
            if trimmed.is_empty() {
                DEFAULT_HOST.to_string()
            } else {
                trimmed.to_string()
            }
        };
        let path = {
            let trimmed = config.path.trim();
            let trimmed = if trimmed.is_empty() {
                DEFAULT_PATH
            } else {
                trimmed
            };
            if trimmed.starts_with('/') {
                trimmed.to_string()
            } else {
                format!("/{trimmed}")
            }
        };
        let bridge_token = config
            .bridge_token
            .as_deref()
            .map(str::trim)
            .filter(|token| !token.is_empty())
            .map(str::to_owned);
        let runtime_session = {
            let trimmed = config.runtime_session.trim();
            if trimmed.is_empty() {
                DEFAULT_RUNTIME_SESSION.to_string()
            } else {
                trimmed.to_string()
            }
        };
        Self {
            host,
            port: config.port,
            path,
            bridge_token,
            runtime_session,
            max_body_bytes: config.max_body_bytes,
            dedup_window_ms: config.dedup_window_ms,
            dedup_cap: config.dedup_cap.max(1),
            activity: ActivityQueue::new(config.activity_queue_cap),
            wake_ids: Mutex::new(VecDeque::new()),
            handler: None,
            connected: false,
            clock,
        }
    }

    /// The chat id wakes land in (`platforms.raft.extra.runtime_session`).
    pub fn runtime_session(&self) -> &str {
        &self.runtime_session
    }

    /// The wake route this adapter answers on.
    pub fn wake_path(&self) -> &str {
        &self.path
    }

    /// Every route the host must mount, in reference order (adapter.py:332-335).
    pub fn routes(&self) -> Vec<String> {
        vec![
            HEALTH_PATH.to_string(),
            self.path.clone(),
            ACTIVITY_PATH.to_string(),
            ACTIVITY_DRAIN_PATH.to_string(),
        ]
    }

    /// The wake URL for a `port` the host bound (adapter.py:377).
    pub fn endpoint(&self, port: u16) -> String {
        BridgeSpec::endpoint(&self.host, port, &self.path)
    }

    /// The bridge command for `program` (the host's `raft` lookup) and `profile`
    /// (`RAFT_PROFILE`); `None` when `profile` is blank, as in the reference.
    pub fn bridge_spec(&self, program: &str, profile: &str) -> Option<BridgeSpec> {
        let token = self.bridge_token.as_deref().unwrap_or_default();
        BridgeSpec::new(program, profile, &self.host, self.port, &self.path, token)
    }

    /// Attach the host's message handler; wakes dispatch into it (hermes
    /// `set_message_handler`). A wake that arrives before this is set answers `503 not_ready`.
    pub fn set_message_handler(&mut self, handler: MessageHandler) {
        self.handler = Some(handler);
    }

    /// Whether [`PlatformAdapter::connect`] succeeded and nothing has disconnected since.
    pub fn is_connected(&self) -> bool {
        self.connected
    }

    /// The activity queue, for a host that wants to inspect or drain it out of band.
    pub fn activity_queue(&self) -> &ActivityQueue {
        &self.activity
    }

    /// Feed one activity event into the queue (hermes `report_activity`, adapter.py:512-516).
    ///
    /// Returns whether the event was queued: the reference logs a validation failure at debug and
    /// carries on, so an invalid event is dropped here too — the boolean just lets the caller log
    /// the same line without the adapter owning a logger.
    pub fn report_activity(&self, event: &Value) -> bool {
        self.activity.push(event).is_ok()
    }

    /// Build the [`MessageEvent`] one already-validated wake payload produces.
    ///
    /// This is the reference's `_handle_wake` event construction (adapter.py:465-471) without the
    /// HTTP envelope: the same fixed [`WAKE_PROMPT`], the same delivery id precedence with the
    /// `raft-wake-{millis}` fallback, and the same source naming.
    pub fn wake_event(&self, payload: &Map<String, Value>) -> MessageEvent {
        let (message_id, _) = self.wake_identity(payload);
        let mut source = SessionSource::new(raft_platform(), self.runtime_session.clone());
        source.chat_name = Some(WAKE_CHAT_NAME.to_string());
        source.chat_type = WAKE_CHAT_TYPE.to_string();
        source.user_id = Some(WAKE_USER_ID.to_string());
        source.user_name = Some(WAKE_USER_NAME.to_string());
        source.message_id = Some(message_id.clone());

        let mut event = MessageEvent::new(WAKE_PROMPT, source);
        event.message_type = MessageType::Text;
        event.user_id = Some(WAKE_USER_ID.to_string());
        event.user_name = Some(WAKE_USER_NAME.to_string());
        event.message_id = Some(message_id);
        event.raw_message = Some(Value::Object(payload.clone()));
        // hermes: `internal=True` — a synthetic event that bypasses inbound auth.
        event.internal = true;
        event
    }

    /// The delivery id for `payload` and whether it is the synthetic fallback.
    fn wake_identity(&self, payload: &Map<String, Value>) -> (String, bool) {
        for key in WAKE_ID_KEYS {
            let Some(value) = payload.get(key) else {
                continue;
            };
            let id = match value {
                Value::String(text) if !text.is_empty() => text.clone(),
                Value::Number(number) if number.as_f64() != Some(0.0) => number.to_string(),
                // Objects/arrays/booleans are not bridge identities; the reference would `str()`
                // them into a Python repr that can never match a retry, this port skips them.
                _ => continue,
            };
            return (id, false);
        }
        (format!("raft-wake-{}", (self.clock)()), true)
    }

    /// Whether this delivery id already woke the gateway inside the dedup window; records it when
    /// it has not.
    fn seen_recently(&self, delivery_id: &str) -> bool {
        let now = (self.clock)();
        let mut seen = lock(&self.wake_ids);
        while let Some((at, _)) = seen.front() {
            if now.saturating_sub(*at) <= self.dedup_window_ms {
                break;
            }
            seen.pop_front();
        }
        if seen.iter().any(|(_, id)| id == delivery_id) {
            return true;
        }
        while seen.len() >= self.dedup_cap {
            seen.pop_front();
        }
        seen.push_back((now, delivery_id.to_string()));
        false
    }

    /// Constant-time check of the bridge token (hermes `_authorized`, adapter.py:424-427).
    fn authorized(&self, request: &WebhookRequest) -> bool {
        let Some(expected) = self.bridge_token.as_deref() else {
            return false;
        };
        let Some(presented) = request.header(BRIDGE_TOKEN_HEADER) else {
            return false;
        };
        if expected.is_empty() || presented.is_empty() {
            return false;
        }
        constant_time_eq(presented.as_bytes(), expected.as_bytes())
    }

    /// The reference's two-step body cap: declared length first, then the bytes actually read
    /// (adapter.py:434-445).
    fn capped_body<'a>(&self, request: &'a WebhookRequest) -> Result<&'a [u8], (u16, String)> {
        let declared = request
            .header("content-length")
            .and_then(|value| value.trim().parse::<usize>().ok());
        if let Some(declared) = declared
            && declared > self.max_body_bytes
        {
            return Err(error_response("payload_too_large", 413));
        }
        if request.body.len() > self.max_body_bytes {
            return Err(error_response("payload_too_large", 413));
        }
        Ok(&request.body)
    }

    /// `GET /health` (adapter.py:419-422). Unauthenticated in the reference; kept that way.
    fn health_response(&self) -> (u16, String) {
        json_response(
            200,
            &json!({
                "status": "ok",
                "platform": PLATFORM_NAME,
                "runtimeSession": self.runtime_session,
                "activity": {
                    "queueSize": self.activity.size(),
                    "endpoint": ACTIVITY_PATH,
                    "drainEndpoint": ACTIVITY_DRAIN_PATH,
                },
            }),
        )
    }

    /// `POST /activity` (adapter.py:479-489): validate, enqueue, `202 {"ok": true}`.
    fn activity_response(&self, request: &WebhookRequest) -> (u16, String) {
        if !self.authorized(request) {
            return error_response("unauthorized", 401);
        }
        let body = match self.capped_body(request) {
            Ok(body) => body,
            Err(response) => return response,
        };
        let Ok(text) = std::str::from_utf8(body) else {
            return error_response("invalid_json", 400);
        };
        let Ok(event) = serde_json::from_str::<Value>(text) else {
            return error_response("invalid_json", 400);
        };
        match self.activity.push(&event) {
            Ok(()) => json_response(202, &json!({ "ok": true })),
            Err(err) => error_response(&err.to_string(), 400),
        }
    }

    /// `GET /activity/drain?max=N` (adapter.py:491-495).
    fn drain_response(&self, query: &str) -> (u16, String) {
        json_response(200, &self.activity.drain(drain_limit(query)))
    }

    /// `POST {path}` (adapter.py:448-477): the wake route.
    async fn wake_response(&self, request: &WebhookRequest) -> (u16, String) {
        if !self.authorized(request) {
            return error_response("unauthorized", 401);
        }
        let body = match self.capped_body(request) {
            Ok(body) => body,
            Err(response) => return response,
        };
        let blank = body.iter().all(u8::is_ascii_whitespace);
        let payload = if blank {
            Value::Object(Map::new())
        } else {
            match serde_json::from_slice::<Value>(body) {
                Ok(payload) => payload,
                Err(_) => return error_response("invalid_json", 400),
            }
        };
        let Some(fields) = payload.as_object() else {
            return error_response("invalid_payload", 400);
        };
        if has_content_field(&payload) {
            return error_response("content_not_allowed", 400);
        }
        let Some(handler) = self.handler.as_ref() else {
            return not_ready_response(&self.runtime_session);
        };
        let (delivery_id, synthetic) = self.wake_identity(fields);
        if !synthetic && self.seen_recently(&delivery_id) {
            // The reference answers 202 for every accepted wake; a repeat inside the window is
            // accepted and dropped, so the bridge does not retry it.
            return json_response(202, &accepted_body(&self.runtime_session, true));
        }
        let event = self.wake_event(fields);
        // A wake is a hint, so the handler's reply has nowhere to go: raft delivers through the
        // raft CLI, never through this adapter.
        let _ = handler(event).await;
        json_response(202, &accepted_body(&self.runtime_session, false))
    }
}

/// The gateway platform handle for this adapter.
///
/// [`PLATFORM_NAME`] is never empty, so the fallback only exists to keep the function total
/// rather than unwrapping.
fn raft_platform() -> Platform {
    Platform::new(PLATFORM_NAME).unwrap_or_else(Platform::local)
}

#[async_trait]
impl PlatformAdapter for RaftAdapter {
    /// Materialise the bridge token and check the endpoint's port, then report readiness.
    ///
    /// The reference generates a 32-byte hex token when none is configured (adapter.py:326-328),
    /// refuses a fixed port that something is already listening on (adapter.py:336-344) and only
    /// then binds. Binding, serving and spawning the bridge are the host's here — see the module
    /// docs — so this returns `true` once the adapter is ready to be served: token materialised,
    /// configured port free. It returns `false` when the OS CSPRNG fails or a fixed port is busy.
    ///
    /// `is_reconnect` changes nothing: the adapter holds no connection to re-establish.
    async fn connect(&mut self, _is_reconnect: bool) -> bool {
        if self.bridge_token.is_none() {
            match random_bridge_token() {
                Some(token) => self.bridge_token = Some(token),
                None => return false,
            }
        }
        if self.port != 0 && port_in_use(self.port) {
            return false;
        }
        self.connected = true;
        true
    }

    /// Drop the connection flag. Sockets and the bridge child belong to the host; the wake dedup
    /// cache is deliberately kept so a bridge that restarts cannot replay a wake inside the window.
    async fn disconnect(&mut self) {
        self.connected = false;
    }

    /// Always fails: raft is wake-only, and the reference's `success=True` no-op would be a
    /// phantom delivery. See [`WAKE_ONLY_SEND_REFUSAL`].
    ///
    /// The error kind is `unknown` because no [`crate::platform::SEND_ERROR_KINDS`] member means
    /// "this transport cannot send at all"; the message is the authoritative explanation and the
    /// result is not retryable.
    async fn send(
        &self,
        _chat_id: &str,
        _content: &str,
        _reply_to: Option<&str>,
        _metadata: Option<Value>,
    ) -> SendResult {
        SendResult::failed(WAKE_ONLY_SEND_REFUSAL, Some("unknown"))
    }

    /// `{"name": "raft/<chat>", "type": "raft"}` (adapter.py:416-417).
    ///
    /// A wake conversation has no directory entry to look up — Raft exposes none and the adapter
    /// never touches message cursors — so there is nothing to call and nothing to invent.
    async fn get_chat_info(&self, chat_id: &str) -> ChatInfo {
        ChatInfo {
            name: format!("raft/{chat_id}"),
            chat_type: CHAT_INFO_TYPE.to_string(),
        }
    }
}

#[async_trait]
impl WebhookHandler for RaftAdapter {
    /// Route one inbound request the way the reference's aiohttp router does
    /// (adapter.py:332-335).
    ///
    /// Route and method are matched first: a known path with the wrong method is `405`, an unknown
    /// path is `404`. aiohttp answers those with its own plain-text body; this port uses the
    /// `{"ok": false, "error": …}` envelope every other route answers with and keeps the status.
    async fn handle(&self, request: WebhookRequest) -> (u16, String) {
        let (path, query) = match request.path.split_once('?') {
            Some((path, query)) => (path, query),
            None => (request.path.as_str(), ""),
        };
        let method = request.method.as_str();
        if path == HEALTH_PATH {
            return match method {
                "GET" => self.health_response(),
                _ => error_response("method_not_allowed", 405),
            };
        }
        if path == ACTIVITY_PATH {
            return match method {
                "POST" => self.activity_response(&request),
                _ => error_response("method_not_allowed", 405),
            };
        }
        if path == ACTIVITY_DRAIN_PATH {
            return match method {
                "GET" => match self.authorized(&request) {
                    true => self.drain_response(query),
                    false => error_response("unauthorized", 401),
                },
                _ => error_response("method_not_allowed", 405),
            };
        }
        if path == self.path {
            return match method {
                "POST" => self.wake_response(&request).await,
                _ => error_response("method_not_allowed", 405),
            };
        }
        error_response("not_found", 404)
    }
}

/// A JSON response envelope `(status, body)`.
fn json_response(status: u16, value: &Value) -> (u16, String) {
    let body = serde_json::to_string(value)
        .unwrap_or_else(|_| "{\"ok\":false,\"error\":\"encode_failed\"}".to_string());
    (status, body)
}

/// hermes `_error_response` (adapter.py:303-304).
fn error_response(error: &str, status: u16) -> (u16, String) {
    json_response(status, &json!({ "ok": false, "error": error }))
}

/// The `503 not_ready` body (adapter.py:461).
fn not_ready_response(runtime_session: &str) -> (u16, String) {
    json_response(
        503,
        &json!({
            "ok": false,
            "error": "not_ready",
            "runtimeSession": runtime_session,
        }),
    )
}

/// The `202` wake body (adapter.py:477).
///
/// `duplicate` is this port's marker for a wake the dedup stage swallowed; it is additive and only
/// ever `true`, so a bridge that checks `ok` sees exactly what the reference sends.
fn accepted_body(runtime_session: &str, duplicate: bool) -> Value {
    if duplicate {
        json!({
            "ok": true,
            "runtimeSession": runtime_session,
            "duplicate": true,
        })
    } else {
        json!({
            "ok": true,
            "runtimeSession": runtime_session,
        })
    }
}

/// True when `value` holds message content at any depth (hermes `_has_content_field`,
/// adapter.py:95-98): a key in [`CONTENT_FIELD_NAMES`], case- and whitespace-insensitively.
fn has_content_field(value: &Value) -> bool {
    match value {
        Value::Object(map) => map.iter().any(|(key, nested)| {
            let key = key.trim().to_lowercase();
            CONTENT_FIELD_NAMES.contains(&key.as_str()) || has_content_field(nested)
        }),
        Value::Array(items) => items.iter().any(has_content_field),
        _ => false,
    }
}

/// `_safe_scalar` (adapter.py:101-103): a non-empty string of at most [`MAX_SCALAR_LENGTH`]
/// characters drawn from `[A-Za-z0-9._:@/ -]`.
fn safe_scalar(value: Option<&Value>) -> Option<&str> {
    let text = value?.as_str()?;
    let length = text.chars().count();
    if length == 0 || length > MAX_SCALAR_LENGTH {
        return None;
    }
    if !text.chars().all(is_safe_scalar_char) {
        return None;
    }
    Some(text)
}

/// The `_SAFE_SCALAR_RE` character set (adapter.py:64).
fn is_safe_scalar_char(ch: char) -> bool {
    ch.is_ascii_alphanumeric() || matches!(ch, '.' | '_' | ':' | '@' | '/' | ' ' | '-')
}

/// `_duration_ms` (adapter.py:116-119): a non-negative number, truncated to whole milliseconds.
fn duration_ms(value: &Value) -> Option<i64> {
    let number = match value {
        Value::Number(number) => number.as_f64()?,
        _ => return None,
    };
    if !number.is_finite() || number < 0.0 {
        return None;
    }
    Some(number.trunc() as i64)
}

/// Validate one activity event (hermes `_validate_activity_event`, adapter.py:148-174).
///
/// The order of the checks is the reference's, so the same malformed event produces the same
/// message: schema, then unknown fields (naming the alphabetically first), then the four required
/// scalars, then `status`, then the optional-field rules, then the content fields and their
/// truncation flags.
pub fn validate_activity_event(value: &Value) -> Result<Value, ActivityError> {
    let Some(fields) = value.as_object() else {
        return Err(ActivityError::NotAnObject);
    };
    if fields.get("schema").and_then(Value::as_str) != Some(ACTIVITY_EVENT_SCHEMA) {
        return Err(ActivityError::UnsupportedSchema);
    }
    let unknown: BTreeSet<&str> = fields
        .keys()
        .map(String::as_str)
        .filter(|key| !ACTIVITY_ALLOWED_FIELDS.contains(key))
        .collect();
    if let Some(first) = unknown.iter().next() {
        return Err(ActivityError::UnknownField((*first).to_string()));
    }
    for key in ["eventId", "sessionId", "hookEventName", "occurredAt"] {
        if safe_scalar(fields.get(key)).is_none() {
            return Err(ActivityError::UnsafeScalar(key));
        }
    }
    if !matches!(
        fields.get("status").and_then(Value::as_str),
        Some("ok" | "error")
    ) {
        return Err(ActivityError::BadStatus);
    }
    for key in ["toolName", "errorClass"] {
        if let Some(raw) = fields.get(key).filter(|raw| !raw.is_null())
            && safe_scalar(Some(raw)).is_none()
        {
            return Err(ActivityError::BadField {
                field: key,
                expected: "a safe string",
            });
        }
    }
    if let Some(raw) = fields.get("durationMs").filter(|raw| !raw.is_null())
        && duration_ms(raw).is_none()
    {
        return Err(ActivityError::BadField {
            field: "durationMs",
            expected: "a non-negative number",
        });
    }
    for key in ["truncated", "toolInputTruncated", "toolOutputTruncated"] {
        if let Some(raw) = fields.get(key).filter(|raw| !raw.is_null())
            && !raw.is_boolean()
        {
            return Err(ActivityError::BadField {
                field: key,
                expected: "a boolean",
            });
        }
    }

    let mut event = fields.clone();
    if let Some(rounded) = event
        .get("durationMs")
        .filter(|raw| !raw.is_null())
        .and_then(duration_ms)
    {
        event.insert("durationMs".to_string(), json!(rounded));
    }
    for key in ["toolInput", "toolOutput"] {
        let Some(content) = event.get(key).filter(|raw| !raw.is_null()) else {
            continue;
        };
        let Some(text) = content.as_str() else {
            return Err(ActivityError::NotText(key));
        };
        if text.chars().count() > ACTIVITY_CONTENT_CAP {
            let truncated: String = text.chars().take(ACTIVITY_CONTENT_CAP).collect();
            event.insert(key.to_string(), Value::String(truncated));
            event.insert("truncated".to_string(), Value::Bool(true));
            event.insert(format!("{key}Truncated"), Value::Bool(true));
        }
    }
    Ok(Value::Object(event))
}

/// `coerce_port(query.get("max", "200"), 200)` for the drain route (adapter.py:494): a
/// non-numeric, negative or zero value falls back to [`DEFAULT_DRAIN_MAX`].
fn drain_limit(query: &str) -> usize {
    let raw = query.split('&').find_map(|pair| {
        let (key, value) = pair.split_once('=')?;
        (key == "max").then_some(value)
    });
    match raw.and_then(|value| value.trim().parse::<i64>().ok()) {
        Some(limit) if limit > 0 => limit as usize,
        _ => DEFAULT_DRAIN_MAX,
    }
}

/// Constant-time byte comparison, the equivalent of `hmac.compare_digest` (adapter.py:427).
///
/// The length is compared first, exactly as `compare_digest` does; the contents are folded into one
/// accumulator so the comparison time does not depend on where the first difference is.
fn constant_time_eq(left: &[u8], right: &[u8]) -> bool {
    if left.len() != right.len() {
        return false;
    }
    let mut difference = 0u8;
    for (a, b) in left.iter().zip(right.iter()) {
        difference |= a ^ b;
    }
    difference == 0
}

/// A fresh 32-byte hex bridge token — hermes `secrets.token_hex(32)` (adapter.py:327) — from the
/// OS CSPRNG. `None` when the platform cannot provide entropy.
fn random_bridge_token() -> Option<String> {
    let mut bytes = [0u8; 32];
    getrandom::fill(&mut bytes).ok()?;
    let mut token = String::with_capacity(bytes.len() * 2);
    for byte in bytes {
        let _ = write!(token, "{byte:02x}");
    }
    Some(token)
}

/// The reference's port probe (adapter.py:338-344): a socket that accepts a connection means
/// something is already serving that port.
fn port_in_use(port: u16) -> bool {
    let address = SocketAddr::from(([127, 0, 0, 1], port));
    TcpStream::connect_timeout(&address, Duration::from_secs(1)).is_ok()
}

/// A mutex that survives poisoning: one panicking request must not take the endpoint down.
fn lock<T>(guard: &Mutex<T>) -> MutexGuard<'_, T> {
    match guard.lock() {
        Ok(guard) => guard,
        Err(poisoned) => poisoned.into_inner(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::atomic::{AtomicU64, Ordering};

    /// A clock the test moves by hand.
    fn fixed_clock(start_millis: u64) -> (Clock, Arc<AtomicU64>) {
        let value = Arc::new(AtomicU64::new(start_millis));
        let handle = Arc::clone(&value);
        (Arc::new(move || handle.load(Ordering::Relaxed)), value)
    }

    /// A handler that records the events it is given and returns no reply.
    fn recording_handler() -> (MessageHandler, Arc<Mutex<Vec<MessageEvent>>>) {
        let sink: Arc<Mutex<Vec<MessageEvent>>> = Arc::new(Mutex::new(Vec::new()));
        let handler_sink = Arc::clone(&sink);
        let handler: MessageHandler = Arc::new(move |event: MessageEvent| {
            let sink = Arc::clone(&handler_sink);
            Box::pin(async move {
                lock(&sink).push(event);
                None
            })
        });
        (handler, sink)
    }

    fn adapter_with(config: RaftConfig, clock: Clock) -> RaftAdapter {
        let mut adapter = RaftAdapter::with_clock(config, clock);
        let (handler, _) = recording_handler();
        adapter.set_message_handler(handler);
        adapter
    }

    /// An adapter whose handler records into a sink the test can read.
    fn adapter_and_sink(
        config: RaftConfig,
        clock: Clock,
    ) -> (RaftAdapter, Arc<Mutex<Vec<MessageEvent>>>) {
        let mut adapter = RaftAdapter::with_clock(config, clock);
        let (handler, sink) = recording_handler();
        adapter.set_message_handler(handler);
        (adapter, sink)
    }

    fn token_config(token: &str) -> RaftConfig {
        RaftConfig {
            bridge_token: Some(token.to_string()),
            ..RaftConfig::default()
        }
    }

    fn post(path: &str, body: impl Into<Vec<u8>>, token: Option<&str>) -> WebhookRequest {
        let mut headers = Vec::new();
        if let Some(token) = token {
            headers.push((BRIDGE_TOKEN_HEADER.to_string(), token.to_string()));
        }
        WebhookRequest {
            method: "POST".to_string(),
            path: path.to_string(),
            headers,
            body: body.into(),
        }
    }

    fn get(path: &str, token: Option<&str>) -> WebhookRequest {
        let mut headers = Vec::new();
        if let Some(token) = token {
            headers.push((BRIDGE_TOKEN_HEADER.to_string(), token.to_string()));
        }
        WebhookRequest {
            method: "GET".to_string(),
            path: path.to_string(),
            headers,
            body: Vec::new(),
        }
    }

    /// The response body of a route call, parsed.
    async fn call(adapter: &RaftAdapter, request: WebhookRequest) -> (u16, Value) {
        let (status, body) = adapter.handle(request).await;
        let value: Value = serde_json::from_str(&body).expect("route bodies are JSON");
        (status, value)
    }

    async fn wake(adapter: &RaftAdapter, payload: Value, token: &str) -> (u16, Value) {
        let body = serde_json::to_vec(&payload).expect("payload encodes");
        call(adapter, post(DEFAULT_PATH, body, Some(token))).await
    }

    // ----- inbound payload shapes -----

    #[tokio::test]
    async fn a_wake_payload_becomes_the_reference_event() {
        let (clock, _) = fixed_clock(1_700_000_000_000);
        let (adapter, sink) = adapter_and_sink(token_config("tok"), clock);
        let payload = json!({ "eventId": "evt-1" });

        let (status, body) = wake(&adapter, payload.clone(), "tok").await;
        assert_eq!(status, 202);
        assert_eq!(body, json!({ "ok": true, "runtimeSession": "default" }));
        assert_eq!(
            body.get("duplicate"),
            None,
            "a fresh wake carries no marker"
        );

        let events = lock(&sink);
        assert_eq!(events.len(), 1);
        let event = &events[0];
        assert_eq!(event.text, WAKE_PROMPT);
        assert_eq!(event.message_type, MessageType::Text);
        assert!(event.internal, "a wake is a synthetic internal event");
        assert_eq!(event.message_id.as_deref(), Some("evt-1"));
        assert_eq!(event.source.message_id.as_deref(), Some("evt-1"));
        assert_eq!(event.source.platform.as_str(), PLATFORM_NAME);
        assert_eq!(event.source.chat_id, "default");
        assert_eq!(event.source.chat_name.as_deref(), Some("Raft channel"));
        assert_eq!(event.source.chat_type, "dm");
        assert_eq!(event.source.user_id.as_deref(), Some("raft-bridge"));
        assert_eq!(event.source.user_name.as_deref(), Some("Raft Bridge"));
        assert_eq!(event.user_id.as_deref(), Some("raft-bridge"));
        assert_eq!(event.user_name.as_deref(), Some("Raft Bridge"));
        assert_eq!(event.raw_message.as_ref(), Some(&payload));
    }

    #[test]
    fn the_wake_prompt_is_the_reference_text_verbatim() {
        assert_eq!(
            WAKE_PROMPT,
            "Raft wake hint received. New Raft messages may be pending. If you have not read the \
Raft manual in this session, run `raft manual get raft-cli-overview` before using Raft commands."
        );
    }

    #[tokio::test]
    async fn delivery_ids_follow_the_reference_precedence() {
        let (clock, _) = fixed_clock(1_700_000_000_000);
        let adapter = adapter_with(token_config("tok"), clock);
        // Every key on its own produces that key's value.
        for key in WAKE_ID_KEYS {
            let payload = json!({ key: format!("id-{key}") });
            let event = adapter.wake_event(payload.as_object().expect("object"));
            assert_eq!(
                event.message_id.as_deref(),
                Some(format!("id-{key}").as_str()),
                "{key} must supply the delivery id"
            );
        }
        // All of them at once: the first key in the list wins.
        let all = json!({
            "id": "sixth",
            "wake_id": "fifth",
            "delivery_id": "fourth",
            "messageId": "third",
            "attemptId": "second",
            "eventId": "first",
        });
        let event = adapter.wake_event(all.as_object().expect("object"));
        assert_eq!(event.message_id.as_deref(), Some("first"));
        // Numbers are ids too.
        let numeric = json!({ "attemptId": 42 });
        assert_eq!(
            adapter
                .wake_event(numeric.as_object().expect("object"))
                .message_id
                .as_deref(),
            Some("42")
        );
    }

    #[tokio::test]
    async fn a_wake_without_id_keys_gets_a_clock_id() {
        let (clock, _) = fixed_clock(1_700_000_000_123);
        let (adapter, sink) = adapter_and_sink(token_config("tok"), clock);
        let (status, _) = wake(&adapter, json!({ "schema": "raft-wake.v1" }), "tok").await;
        assert_eq!(status, 202);
        assert_eq!(
            lock(&sink)[0].message_id.as_deref(),
            Some("raft-wake-1700000000123")
        );
    }

    #[tokio::test]
    async fn wake_notify_and_heartbeat_shapes_all_become_one_fixed_wake() {
        // The reference reads no event-kind field: the payload is only ever a hint, so every shape
        // produces the same fixed text and differs only in its delivery id.
        let (clock, _) = fixed_clock(1_700_000_000_000);
        let (adapter, sink) = adapter_and_sink(token_config("tok"), clock);
        for payload in [
            json!({ "event": "wake", "eventId": "wake-1" }),
            json!({ "event": "notify", "eventId": "notify-1", "attemptId": "att-1" }),
            json!({ "type": "heartbeat", "id": "hb-7", "at": "2024-01-02T03:04:05Z" }),
        ] {
            let (status, _) = wake(&adapter, payload, "tok").await;
            assert_eq!(status, 202);
        }
        let events = lock(&sink);
        assert_eq!(events.len(), 3);
        let ids: Vec<&str> = events
            .iter()
            .map(|event| event.message_id.as_deref().unwrap_or_default())
            .collect();
        assert_eq!(ids, vec!["wake-1", "notify-1", "hb-7"]);
        for event in events.iter() {
            assert_eq!(
                event.text, WAKE_PROMPT,
                "no payload text may reach the event"
            );
        }
    }

    #[tokio::test]
    async fn payload_metadata_never_reaches_the_wake_text() {
        let (clock, _) = fixed_clock(1_700_000_000_000);
        let (adapter, sink) = adapter_and_sink(token_config("tok"), clock);
        let payload = json!({
            "eventId": "evt-2",
            "channel": "ops",
            "sender": "alice",
            "count": 3,
        });
        assert_eq!(wake(&adapter, payload.clone(), "tok").await.0, 202);
        let events = lock(&sink);
        assert_eq!(events[0].text, WAKE_PROMPT);
        assert_eq!(events[0].raw_message.as_ref(), Some(&payload));
    }

    #[tokio::test]
    async fn the_runtime_session_names_the_wake_chat() {
        let (clock, _) = fixed_clock(1_700_000_000_000);
        let config = RaftConfig {
            bridge_token: Some("tok".to_string()),
            runtime_session: "ops".to_string(),
            path: "wake".to_string(),
            ..RaftConfig::default()
        };
        let (adapter, sink) = adapter_and_sink(config, clock);
        assert_eq!(adapter.wake_path(), "/wake", "a path gains its leading /");
        assert_eq!(adapter.runtime_session(), "ops");
        let (status, body) = wake(&adapter, json!({ "eventId": "e" }), "tok").await;
        assert_eq!(status, 202);
        assert_eq!(body["runtimeSession"], "ops");
        assert_eq!(lock(&sink)[0].source.chat_id, "ops");
    }

    // ----- refusals and malformed input -----

    #[tokio::test]
    async fn content_bearing_payloads_are_refused() {
        let (clock, _) = fixed_clock(1_700_000_000_000);
        let (adapter, sink) = adapter_and_sink(token_config("tok"), clock);
        for payload in [
            json!({ "eventId": "e1", "body": "the actual message" }),
            json!({ "eventId": "e2", "text": "hello" }),
            json!({ "eventId": "e3", "nested": { "snippet": "peek" } }),
            json!({ "eventId": "e4", "list": [{ "preview": "x" }] }),
            json!({ "EventId": "e5", "CONTENT": "shouty" }),
        ] {
            let (status, body) = wake(&adapter, payload, "tok").await;
            assert_eq!(status, 400);
            assert_eq!(body, json!({ "ok": false, "error": "content_not_allowed" }));
        }
        assert!(
            lock(&sink).is_empty(),
            "a content payload must never open a turn"
        );
    }

    #[tokio::test]
    async fn malformed_bodies_are_rejected_without_panicking() {
        let (clock, _) = fixed_clock(1_700_000_000_000);
        let (adapter, sink) = adapter_and_sink(token_config("tok"), clock);

        let (status, body) = call(&adapter, post(DEFAULT_PATH, "{not json", Some("tok"))).await;
        assert_eq!(status, 400);
        assert_eq!(body["error"], "invalid_json");

        let (status, body) = call(&adapter, post(DEFAULT_PATH, "[1,2,3]", Some("tok"))).await;
        assert_eq!(status, 400);
        assert_eq!(body["error"], "invalid_payload");

        let (status, body) = call(&adapter, post(DEFAULT_PATH, "\"a string\"", Some("tok"))).await;
        assert_eq!(status, 400);
        assert_eq!(body["error"], "invalid_payload");

        let (status, body) =
            call(&adapter, post(DEFAULT_PATH, vec![0xff, 0xfe], Some("tok"))).await;
        assert_eq!(status, 400);
        assert_eq!(body["error"], "invalid_json");
        assert!(lock(&sink).is_empty());
    }

    #[tokio::test]
    async fn a_blank_body_is_an_empty_object() {
        let (clock, _) = fixed_clock(1_700_000_000_000);
        let (adapter, sink) = adapter_and_sink(token_config("tok"), clock);
        for body in ["", "  \n\t "] {
            let (status, _) = call(&adapter, post(DEFAULT_PATH, body, Some("tok"))).await;
            assert_eq!(status, 202, "the reference parses a blank body as {{}}");
        }
        // Two blank bodies get the same synthetic id for this frozen clock, and synthetic ids are
        // never deduplicated, so both are dispatched.
        assert_eq!(lock(&sink).len(), 2);
    }

    #[tokio::test]
    async fn a_missing_or_wrong_bridge_token_is_unauthorized() {
        let (clock, _) = fixed_clock(1_700_000_000_000);
        let (adapter, sink) = adapter_and_sink(token_config("tok"), clock);
        let body = br#"{"eventId":"e"}"#.to_vec();

        for token in [None, Some("nope"), Some(""), Some("TOK")] {
            let (status, response) = call(&adapter, post(DEFAULT_PATH, body.clone(), token)).await;
            assert_eq!(status, 401, "token {token:?} must be refused");
            assert_eq!(response, json!({ "ok": false, "error": "unauthorized" }));
        }
        assert!(lock(&sink).is_empty());

        // An adapter that has not materialised a token yet refuses everything.
        let fresh = adapter_with(RaftConfig::default(), fixed_clock(0).0);
        let (status, _) = call(&fresh, post(DEFAULT_PATH, body, Some("tok"))).await;
        assert_eq!(status, 401);
    }

    #[tokio::test]
    async fn health_needs_no_token_and_reports_the_activity_queue() {
        let (clock, _) = fixed_clock(1_700_000_000_000);
        let adapter = adapter_with(token_config("tok"), clock);
        let (status, body) = call(&adapter, get(HEALTH_PATH, None)).await;
        assert_eq!(status, 200);
        assert_eq!(
            body,
            json!({
                "status": "ok",
                "platform": "raft",
                "runtimeSession": "default",
                "activity": {
                    "queueSize": 0,
                    "endpoint": "/activity",
                    "drainEndpoint": "/activity/drain",
                },
            })
        );
    }

    #[tokio::test]
    async fn oversized_bodies_are_refused_before_parsing() {
        let (clock, _) = fixed_clock(1_700_000_000_000);
        let config = RaftConfig {
            bridge_token: Some("tok".to_string()),
            max_body_bytes: 32,
            ..RaftConfig::default()
        };
        let (adapter, sink) = adapter_and_sink(config, clock);

        // The bytes read exceed the cap.
        let big = "x".repeat(64);
        let (status, body) = call(&adapter, post(DEFAULT_PATH, big.clone(), Some("tok"))).await;
        assert_eq!(status, 413);
        assert_eq!(body["error"], "payload_too_large");

        // A declared Content-Length exceeds the cap even though the bytes do not.
        let mut request = post(DEFAULT_PATH, "{}", Some("tok"));
        request
            .headers
            .push(("Content-Length".to_string(), "4096".to_string()));
        let (status, body) = call(&adapter, request).await;
        assert_eq!(status, 413);
        assert_eq!(body["error"], "payload_too_large");
        assert!(lock(&sink).is_empty());
    }

    #[tokio::test]
    async fn a_wake_before_a_handler_is_attached_is_not_ready() {
        let (clock, _) = fixed_clock(1_700_000_000_000);
        let adapter = RaftAdapter::with_clock(token_config("tok"), clock);
        let (status, body) = wake(&adapter, json!({ "eventId": "e" }), "tok").await;
        assert_eq!(status, 503);
        assert_eq!(
            body,
            json!({ "ok": false, "error": "not_ready", "runtimeSession": "default" })
        );
    }

    // ----- dedup / cooldown -----

    #[tokio::test]
    async fn a_repeat_wake_inside_the_window_is_dropped() {
        let (clock, hand) = fixed_clock(1_000);
        let (adapter, sink) = adapter_and_sink(token_config("tok"), clock);
        let payload = json!({ "eventId": "evt-1" });

        assert_eq!(wake(&adapter, payload.clone(), "tok").await.0, 202);
        // Inside the window (1s later, window is 5 minutes): still 202, no second turn.
        hand.store(2_000, Ordering::Relaxed);
        let (status, body) = wake(&adapter, payload.clone(), "tok").await;
        assert_eq!(status, 202, "the bridge must not see a retryable error");
        assert_eq!(body["duplicate"], true);
        assert_eq!(lock(&sink).len(), 1);

        // Outside the window: the id has aged out, so the wake opens a new turn.
        hand.store(1_000 + DEFAULT_WAKE_DEDUP_WINDOW_MS + 1, Ordering::Relaxed);
        let (status, body) = wake(&adapter, payload, "tok").await;
        assert_eq!(status, 202);
        assert_eq!(body.get("duplicate"), None);
        assert_eq!(lock(&sink).len(), 2);
    }

    #[tokio::test]
    async fn distinct_ids_inside_the_window_all_wake() {
        let (clock, _) = fixed_clock(1_000);
        let (adapter, sink) = adapter_and_sink(token_config("tok"), clock);
        for id in ["a", "b", "c"] {
            assert_eq!(wake(&adapter, json!({ "eventId": id }), "tok").await.0, 202);
        }
        assert_eq!(
            lock(&sink).len(),
            3,
            "the reference has no cooldown: a burst is a burst of turns"
        );
    }

    #[tokio::test]
    async fn wakes_without_an_id_are_never_deduplicated() {
        let (clock, _) = fixed_clock(1_000);
        let (adapter, sink) = adapter_and_sink(token_config("tok"), clock);
        // Same frozen millisecond, so the same synthetic id — and still two turns, because a
        // synthetic id is not the bridge's delivery identity.
        for _ in 0..2 {
            assert_eq!(wake(&adapter, json!({}), "tok").await.0, 202);
        }
        assert_eq!(lock(&sink).len(), 2);
    }

    #[tokio::test]
    async fn the_dedup_cache_stays_bounded() {
        let (clock, _) = fixed_clock(1_000);
        let config = RaftConfig {
            bridge_token: Some("tok".to_string()),
            dedup_cap: 2,
            ..RaftConfig::default()
        };
        let (adapter, sink) = adapter_and_sink(config, clock);
        for id in ["a", "b", "c"] {
            assert_eq!(wake(&adapter, json!({ "eventId": id }), "tok").await.0, 202);
        }
        assert_eq!(lock(&sink).len(), 3);
        assert!(lock(&adapter.wake_ids).len() <= 2);
    }

    // ----- send / chat info / connect -----

    #[tokio::test]
    async fn send_fails_with_the_wake_only_reason() {
        let adapter = adapter_with(token_config("tok"), fixed_clock(0).0);
        let result = adapter.send("default", "hello", None, None).await;
        assert!(!result.success, "raft has no outbound message API");
        assert_eq!(result.error.as_deref(), Some(WAKE_ONLY_SEND_REFUSAL));
        assert_eq!(result.error_kind.as_deref(), Some("unknown"));
        assert!(result.message_id.is_none());
        assert!(!result.retryable);
        assert!(result.continuation_message_ids.is_empty());
    }

    #[tokio::test]
    async fn chat_info_names_the_wake_conversation() {
        let adapter = adapter_with(token_config("tok"), fixed_clock(0).0);
        let info = adapter.get_chat_info("default").await;
        assert_eq!(info.name, "raft/default");
        assert_eq!(info.chat_type, "raft");
        // The wake source and the chat info disagree in the reference; both are kept.
        assert_eq!(info.chat_type, CHAT_INFO_TYPE);
    }

    #[tokio::test]
    async fn connect_materialises_a_hex_bridge_token() {
        let mut adapter = RaftAdapter::with_clock(RaftConfig::default(), fixed_clock(0).0);
        assert!(adapter.connect(false).await);
        assert!(adapter.is_connected());
        let token = adapter.bridge_token.clone().expect("a token was generated");
        assert_eq!(token.len(), 64, "secrets.token_hex(32)");
        assert!(token.chars().all(|ch| ch.is_ascii_hexdigit()), "{token}");

        adapter.disconnect().await;
        assert!(!adapter.is_connected());
        // A reconnect keeps the token it already had.
        assert!(adapter.connect(true).await);
        assert_eq!(adapter.bridge_token.as_deref(), Some(token.as_str()));
    }

    #[tokio::test]
    async fn connect_keeps_a_configured_token_and_normalises_config() {
        let config = RaftConfig {
            host: "  ".to_string(),
            path: "   ".to_string(),
            bridge_token: Some("  configured  ".to_string()),
            runtime_session: " ".to_string(),
            ..RaftConfig::default()
        };
        let mut adapter = RaftAdapter::with_clock(config, fixed_clock(0).0);
        assert!(adapter.connect(false).await);
        assert_eq!(adapter.bridge_token.as_deref(), Some("configured"));
        assert_eq!(adapter.host, DEFAULT_HOST);
        assert_eq!(adapter.wake_path(), DEFAULT_PATH);
        assert_eq!(adapter.runtime_session(), DEFAULT_RUNTIME_SESSION);
    }

    #[tokio::test]
    async fn connect_refuses_a_port_that_is_already_served() {
        let listener = std::net::TcpListener::bind(("127.0.0.1", 0)).expect("bind a loopback port");
        let port = listener.local_addr().expect("local addr").port();
        let config = RaftConfig {
            port,
            bridge_token: Some("tok".to_string()),
            ..RaftConfig::default()
        };
        let mut adapter = RaftAdapter::with_clock(config, fixed_clock(0).0);
        assert!(
            !adapter.connect(false).await,
            "the reference refuses a port that is already listening"
        );
        assert!(!adapter.is_connected());
        drop(listener);
    }

    // ----- routes -----

    #[tokio::test]
    async fn the_route_table_matches_the_reference() {
        let adapter = adapter_with(token_config("tok"), fixed_clock(0).0);
        assert_eq!(
            adapter.routes(),
            vec![
                "/health".to_string(),
                DEFAULT_PATH.to_string(),
                "/activity".to_string(),
                "/activity/drain".to_string(),
            ]
        );
        assert_eq!(adapter.endpoint(4321), "http://127.0.0.1:4321/wake");
    }

    #[tokio::test]
    async fn unknown_paths_and_wrong_methods_are_matched_first() {
        let adapter = adapter_with(token_config("tok"), fixed_clock(0).0);

        let (status, body) = call(&adapter, get("/nope", None)).await;
        assert_eq!(status, 404);
        assert_eq!(body["error"], "not_found");

        let (status, body) = call(&adapter, get(DEFAULT_PATH, Some("tok"))).await;
        assert_eq!(status, 405, "GET on the wake route");
        assert_eq!(body["error"], "method_not_allowed");

        let (status, _) = call(&adapter, post(HEALTH_PATH, "{}", Some("tok"))).await;
        assert_eq!(status, 405);
    }

    // ----- activity telemetry -----

    fn activity_event() -> Value {
        json!({
            "schema": ACTIVITY_EVENT_SCHEMA,
            "eventId": "hermes-1",
            "sessionId": "sess-1",
            "hookEventName": "PreToolUse",
            "status": "ok",
            "occurredAt": "2024-01-02T03:04:05Z",
            "toolName": "bash",
            "toolInput": "ls -la",
        })
    }

    #[tokio::test]
    async fn activity_events_round_trip_through_push_and_drain() {
        let (clock, _) = fixed_clock(0);
        let adapter = adapter_with(token_config("tok"), clock);
        let event = activity_event();
        let body = serde_json::to_vec(&event).expect("encodes");
        let (status, response) = call(&adapter, post("/activity", body, Some("tok"))).await;
        assert_eq!(status, 202);
        assert_eq!(response, json!({ "ok": true }));

        let (status, health) = call(&adapter, get(HEALTH_PATH, None)).await;
        assert_eq!(status, 200);
        assert_eq!(health["activity"]["queueSize"], 1);

        let (status, drained) = call(&adapter, get("/activity/drain", Some("tok"))).await;
        assert_eq!(status, 200);
        assert_eq!(drained["schema"], ACTIVITY_DRAIN_SCHEMA);
        assert_eq!(drained["events"], json!([event]));
        assert_eq!(drained["dropped"], 0);

        // A second drain is empty.
        let (_, drained) = call(&adapter, get("/activity/drain", Some("tok"))).await;
        assert_eq!(drained["events"], json!([]));
    }

    #[tokio::test]
    async fn activity_push_and_drain_are_authenticated() {
        let adapter = adapter_with(token_config("tok"), fixed_clock(0).0);
        let body = serde_json::to_vec(&activity_event()).expect("encodes");
        let (status, _) = call(&adapter, post("/activity", body, None)).await;
        assert_eq!(status, 401);
        let (status, body) = call(&adapter, get("/activity/drain", None)).await;
        assert_eq!(status, 401);
        assert_eq!(body["error"], "unauthorized");
    }

    #[tokio::test]
    async fn a_rejected_activity_event_reports_the_validation_message() {
        let adapter = adapter_with(token_config("tok"), fixed_clock(0).0);
        let mut broken = activity_event();
        broken["unexpectedField"] = json!("nope");
        let body = serde_json::to_vec(&broken).expect("encodes");
        let (status, response) = call(&adapter, post("/activity", body, Some("tok"))).await;
        assert_eq!(status, 400);
        assert_eq!(
            response,
            json!({ "ok": false, "error": "activity event field unexpectedField is not allowed" })
        );

        let (status, response) = call(&adapter, post("/activity", "not json", Some("tok"))).await;
        assert_eq!(status, 400);
        assert_eq!(response["error"], "invalid_json");
    }

    #[tokio::test]
    async fn the_activity_queue_drops_the_oldest_and_counts_it() {
        let (clock, _) = fixed_clock(0);
        let config = RaftConfig {
            bridge_token: Some("tok".to_string()),
            activity_queue_cap: 2,
            ..RaftConfig::default()
        };
        let adapter = adapter_with(config, clock);
        for index in 0..3 {
            let mut event = activity_event();
            event["eventId"] = json!(format!("hermes-{index}"));
            assert!(adapter.report_activity(&event));
        }
        assert_eq!(adapter.activity_queue().size(), 2);
        let drained = adapter.activity_queue().drain(0);
        assert_eq!(drained["dropped"], 1);
        assert_eq!(drained["events"].as_array().map(Vec::len), Some(2));
        assert_eq!(drained["events"][0]["eventId"], "hermes-1");
    }

    #[test]
    fn activity_validation_follows_the_reference_rules() {
        assert_eq!(
            validate_activity_event(&json!("nope")),
            Err(ActivityError::NotAnObject)
        );
        assert_eq!(
            validate_activity_event(&json!({ "schema": "other" })),
            Err(ActivityError::UnsupportedSchema)
        );

        // Unknown fields name the alphabetically first offender.
        let mut unknown = activity_event();
        unknown["zeta"] = json!(1);
        unknown["alpha"] = json!(1);
        assert_eq!(
            validate_activity_event(&unknown),
            Err(ActivityError::UnknownField("alpha".to_string()))
        );

        // Required scalars must be safe strings.
        let mut unsafe_id = activity_event();
        unsafe_id["sessionId"] = json!("bad\nnewline");
        assert_eq!(
            validate_activity_event(&unsafe_id),
            Err(ActivityError::UnsafeScalar("sessionId"))
        );
        let mut long_id = activity_event();
        long_id["eventId"] = json!("e".repeat(MAX_SCALAR_LENGTH + 1));
        assert_eq!(
            validate_activity_event(&long_id),
            Err(ActivityError::UnsafeScalar("eventId"))
        );

        let mut bad_status = activity_event();
        bad_status["status"] = json!("maybe");
        assert_eq!(
            validate_activity_event(&bad_status),
            Err(ActivityError::BadStatus)
        );

        let mut bad_duration = activity_event();
        bad_duration["durationMs"] = json!(-1);
        assert_eq!(
            validate_activity_event(&bad_duration),
            Err(ActivityError::BadField {
                field: "durationMs",
                expected: "a non-negative number",
            })
        );

        let mut bad_flag = activity_event();
        bad_flag["truncated"] = json!("yes");
        assert_eq!(
            validate_activity_event(&bad_flag),
            Err(ActivityError::BadField {
                field: "truncated",
                expected: "a boolean",
            })
        );

        let mut bad_tool = activity_event();
        bad_tool["toolInput"] = json!({ "not": "text" });
        assert_eq!(
            validate_activity_event(&bad_tool),
            Err(ActivityError::NotText("toolInput"))
        );
    }

    #[test]
    fn activity_validation_truncates_content_and_normalises_duration() {
        let mut event = activity_event();
        event["toolOutput"] = json!("y".repeat(ACTIVITY_CONTENT_CAP + 10));
        event["durationMs"] = json!(12.9);
        let validated = validate_activity_event(&event).expect("valid");
        assert_eq!(
            validated["toolOutput"]
                .as_str()
                .map(|text| text.chars().count()),
            Some(ACTIVITY_CONTENT_CAP)
        );
        assert_eq!(validated["truncated"], json!(true));
        assert_eq!(validated["toolOutputTruncated"], json!(true));
        assert_eq!(validated["durationMs"], json!(12));

        // Content within the cap is left exactly as it was.
        let small = validate_activity_event(&activity_event()).expect("valid");
        assert_eq!(small["toolInput"], "ls -la");
        assert_eq!(small.get("truncated"), None);
    }

    #[test]
    fn nested_content_detection_is_recursive_and_case_insensitive() {
        assert!(has_content_field(&json!({ "body": "x" })));
        assert!(has_content_field(&json!({ "TEXT": "x" })));
        assert!(has_content_field(
            &json!({ "a": { "b": [{ "snippet": 1 }] } })
        ));
        assert!(!has_content_field(&json!({ "eventId": "e", "count": 2 })));
        assert!(!has_content_field(&json!({ "messagesPending": 3 })));
    }

    // ----- bridge wiring -----

    #[test]
    fn the_bridge_command_matches_the_reference_argv_and_env() {
        let spec = BridgeSpec::new("raft", "ops", "127.0.0.1", 8080, "/wake", "tok")
            .expect("a profile was supplied");
        assert_eq!(spec.program, "raft");
        assert_eq!(
            spec.args,
            vec![
                "--profile",
                "ops",
                "agent",
                "bridge",
                "--wake-adapter",
                "wake-channel",
                "--wake-channel-endpoint",
                "http://127.0.0.1:8080/wake",
            ]
        );
        assert_eq!(
            spec.env,
            vec![
                ("RAFT_PROFILE".to_string(), "ops".to_string()),
                ("RAFT_CHANNEL_TOKEN".to_string(), "tok".to_string()),
            ]
        );
        // No profile, no bridge — the reference refuses to spawn.
        assert!(BridgeSpec::new("raft", "   ", "127.0.0.1", 8080, "/wake", "tok").is_none());
    }

    #[tokio::test]
    async fn the_adapter_hands_out_its_bridge_command() {
        let mut adapter = RaftAdapter::with_clock(
            RaftConfig {
                port: 9000,
                ..RaftConfig::default()
            },
            fixed_clock(0).0,
        );
        assert!(adapter.connect(false).await);
        let spec = adapter
            .bridge_spec("raft", "ops")
            .expect("a bridge spec once connected");
        assert_eq!(
            spec.args.last().map(String::as_str),
            Some("http://127.0.0.1:9000/wake")
        );
        assert!(adapter.bridge_spec("raft", "").is_none());
    }

    #[test]
    fn safe_scalars_and_constant_time_compare_behave() {
        assert_eq!(safe_scalar(Some(&json!("evt-1"))), Some("evt-1"));
        assert_eq!(
            safe_scalar(Some(&json!("a b/c@d:e_f.g-h"))),
            Some("a b/c@d:e_f.g-h")
        );
        assert_eq!(safe_scalar(Some(&json!(""))), None);
        assert_eq!(safe_scalar(Some(&json!(7))), None);
        assert_eq!(safe_scalar(Some(&json!("tab\there"))), None);
        assert_eq!(safe_scalar(None), None);

        assert!(constant_time_eq(b"token", b"token"));
        assert!(!constant_time_eq(b"token", b"tokeN"));
        assert!(!constant_time_eq(b"token", b"token "));
        assert!(!constant_time_eq(b"", b"x"));
    }

    #[test]
    fn drain_limits_parse_like_coerce_port() {
        assert_eq!(drain_limit(""), DEFAULT_DRAIN_MAX);
        assert_eq!(drain_limit("max=5"), 5);
        assert_eq!(drain_limit("other=1&max=7"), 7);
        assert_eq!(drain_limit("max=nope"), DEFAULT_DRAIN_MAX);
        assert_eq!(drain_limit("max=-3"), DEFAULT_DRAIN_MAX);
        assert_eq!(drain_limit("max=0"), DEFAULT_DRAIN_MAX);
        assert_eq!(drain_limit("max="), DEFAULT_DRAIN_MAX);
    }
}
