//! The platform adapter contract (module 3 of the foundation).
//!
//! Port of the pieces of `gateway/platforms/base.py` that define the boundary every messaging
//! adapter sits behind: [`SendResult`], [`TurnReply`] / [`MessageHandler`], [`ChatInfo`], the
//! [`PlatformAdapter`] trait itself and the capability-flag surface. Rendering, chunking and the
//! delivery ledger live in `helpers.rs` / `delivery.rs`; approvals go through the runtime
//! approval path, not through this trait.
//!
//! Fidelity notes (full accounting in docs/gateway-port-plan.md):
//! - hermes `SendResult` is a dataclass with `success` required; kept, with builders.
//! - `raw_response` stays a JSON value. The partial-overflow contract (Telegram edit-split
//!   replies set `partial_overflow` / `delivered_chunks` / `total_chunks`) rides on it, so it is
//!   present from day one rather than bolted on.
//! - `continuation_message_ids` are extra ids in send order; `message_id` is the LAST id so later
//!   edits target the newest chunk.
//! - hermes `EphemeralReply(str)` magic becomes an explicit [`TurnReply::Ephemeral`] variant.
//! - `SEND_ERROR_KINDS` is a plain const slice; classification lives in `delivery.rs`.

use async_trait::async_trait;
use serde_json::Value;
use std::future::Future;
use std::pin::Pin;
use std::sync::Arc;

use crate::event::{MessageEvent, SessionSource};

/// SEND failure taxonomy. Presence means "this is a failure"; consumers branch on the kind
/// instead of substring-matching provider error text.
pub const SEND_ERROR_KINDS: &[&str] = &[
    "too_long",
    "bad_format",
    "forbidden",
    "not_found",
    "rate_limited",
    "transient",
    "unknown",
];

/// Server-requested retry delay (Telegram FloodWait) is honored inline only up to this cap;
/// longer delays become a typed failure so the delivery ledger owns the wait.
pub const SEND_RETRY_INLINE_WAIT_CAP_SECS: f64 = 60.0;

/// What a handler may hand back for a turn.
///
/// hermes: `str` (reply), `EphemeralReply` (auto-deletes via `delete_message` after the TTL),
/// or `None` (the handler already delivered — e.g. it called `send` itself).
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum TurnReply {
    /// Plain text reply, sent through the normal delivery pipeline.
    Text(String),
    /// Text that auto-deletes `ttl_seconds` later (`None` = platform default,
    /// `Some(0)` = never delete). Platforms without `delete_message` leave it in place.
    Ephemeral {
        text: String,
        ttl_seconds: Option<u32>,
    },
}

impl TurnReply {
    pub fn text(text: impl Into<String>) -> Self {
        Self::Text(text.into())
    }

    pub fn ephemeral(text: impl Into<String>, ttl_seconds: Option<u32>) -> Self {
        Self::Ephemeral {
            text: text.into(),
            ttl_seconds,
        }
    }

    pub fn body(&self) -> &str {
        match self {
            Self::Text(t) => t,
            Self::Ephemeral { text, .. } => text,
        }
    }

    pub fn ephemeral_ttl(&self) -> Option<Option<u32>> {
        match self {
            Self::Text(_) => None,
            Self::Ephemeral { ttl_seconds, .. } => Some(*ttl_seconds),
        }
    }
}

/// The handler signature hermes wires into adapters via `set_message_handler`.
///
/// hermes: `Callable[[MessageEvent], Awaitable[Optional[Union[str, EphemeralReply]]]]`.
pub type MessageHandler = Arc<
    dyn Fn(MessageEvent) -> Pin<Box<dyn Future<Output = Option<TurnReply>> + Send>> + Send + Sync,
>;

/// What `send` reports back. hermes `SendResult` (base.py:1596).
#[derive(Debug, Clone, PartialEq)]
pub struct SendResult {
    pub success: bool,
    pub message_id: Option<String>,
    pub error: Option<String>,
    /// Adapter-specific payload. Telegram edit-split replies set
    /// `partial_overflow = true`, `delivered_chunks` / `total_chunks` so the stream
    /// consumer sends only the tail, never the already-on-screen head.
    pub raw_response: Option<Value>,
    pub retryable: bool,
    /// Server-requested delay; beats backoff when present (FloodWait).
    pub retry_after: Option<f64>,
    /// Ids in send order when the payload was split; `message_id` is the LAST of them.
    pub continuation_message_ids: Vec<String>,
    /// One of [`SEND_ERROR_KINDS`]; set on failures only.
    pub error_kind: Option<String>,
}

impl SendResult {
    pub fn ok(message_id: impl Into<String>) -> Self {
        Self {
            success: true,
            message_id: Some(message_id.into()),
            error: None,
            raw_response: None,
            retryable: false,
            retry_after: None,
            continuation_message_ids: Vec::new(),
            error_kind: None,
        }
    }

    pub fn ok_split(message_id: impl Into<String>, continuation: Vec<String>) -> Self {
        Self {
            continuation_message_ids: continuation,
            ..Self::ok(message_id)
        }
    }

    /// `error_kind` should be one of [`SEND_ERROR_KINDS`]; passing anything else is accepted
    /// but callers should not rely on it.
    pub fn failed(error: impl Into<String>, error_kind: Option<&str>) -> Self {
        Self {
            success: false,
            message_id: None,
            error: Some(error.into()),
            raw_response: None,
            retryable: false,
            retry_after: None,
            continuation_message_ids: Vec::new(),
            error_kind: error_kind.map(str::to_owned),
        }
    }

    pub fn retryable(mut self, retry_after: Option<f64>) -> Self {
        self.retryable = true;
        self.retry_after = retry_after;
        self
    }

    /// Attach the partial-overflow marker the stream consumer depends on.
    pub fn with_partial_overflow(mut self, delivered_chunks: u32, total_chunks: u32) -> Self {
        let raw = serde_json::json!({
            "partial_overflow": true,
            "delivered_chunks": delivered_chunks,
            "total_chunks": total_chunks,
        });
        self.raw_response = Some(raw);
        self
    }

    pub fn partial_overflow(&self) -> Option<(u32, u32)> {
        let raw = self.raw_response.as_ref()?;
        if raw.get("partial_overflow")?.as_bool()? {
            Some((
                raw.get("delivered_chunks")?.as_u64()? as u32,
                raw.get("total_chunks")?.as_u64()? as u32,
            ))
        } else {
            None
        }
    }
}

/// `get_chat_info` result. hermes returns a plain dict with at least `name` and `type`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ChatInfo {
    pub name: String,
    /// "dm" | "group" | "channel" (platforms may add more; consumers treat unknown
    /// values leniently).
    pub chat_type: String,
}

/// UTF-16 code unit count, used by Telegram where the 4096-char cap counts UTF-16 units.
pub fn utf16_len(text: &str) -> usize {
    text.chars().map(char::len_utf16).sum()
}

/// What an outbound attachment is.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum MediaKind {
    Image,
    Video,
    Audio,
    Document,
}

/// Where the bytes come from: already-held bytes, or a URL the platform fetches itself.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum MediaSource {
    Bytes(Vec<u8>),
    Url(String),
}

/// One outbound attachment. `caption` travels with the media where the platform supports it;
/// otherwise the caller sends it as a follow-up text message.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct OutboundMedia {
    pub kind: MediaKind,
    pub source: MediaSource,
    pub filename: Option<String>,
    pub mime: Option<String>,
    pub caption: Option<String>,
}

impl OutboundMedia {
    pub fn new(kind: MediaKind, source: MediaSource) -> Self {
        Self {
            kind,
            source,
            filename: None,
            mime: None,
            caption: None,
        }
    }

    pub fn image_url(url: impl Into<String>) -> Self {
        Self::new(MediaKind::Image, MediaSource::Url(url.into()))
    }

    pub fn with_filename(mut self, filename: impl Into<String>) -> Self {
        self.filename = Some(filename.into());
        self
    }

    pub fn with_mime(mut self, mime: impl Into<String>) -> Self {
        self.mime = Some(mime.into());
        self
    }

    pub fn with_caption(mut self, caption: impl Into<String>) -> Self {
        self.caption = Some(caption.into());
        self
    }
}

/// The four operations every adapter MUST implement, plus the capability-flag surface and a
/// minimal default implementation of the optional send path.
#[async_trait]

pub trait PlatformAdapter: Send + Sync {
    // ----- required (hermes: @abstractmethod) -----

    /// Establish the connection. `is_reconnect` marks a re-connect after the watcher noticed
    /// the platform dropped; adapters with a server-side update queue (Telegram) must preserve it.
    async fn connect(&mut self, is_reconnect: bool) -> bool;

    async fn disconnect(&mut self);

    /// Send text content (markdown-flavored; platforms normalize or refuse).
    async fn send(
        &self,
        chat_id: &str,
        content: &str,
        reply_to: Option<&str>,
        metadata: Option<Value>,
    ) -> SendResult;

    /// At least `name` and `type` ("dm" | "group" | "channel").
    async fn get_chat_info(&self, chat_id: &str) -> ChatInfo;

    /// Whether [`send_media`](Self::send_media) is implemented for this platform.
    fn supports_media(&self) -> bool {
        false
    }

    /// Whether [`add_reaction`](Self::add_reaction) is implemented for this platform.
    fn supports_reactions(&self) -> bool {
        false
    }

    /// Add an emoji reaction to `message_id`. The default refuses: a platform without a reaction
    /// API must not report that it reacted.
    async fn add_reaction(&self, _chat_id: &str, _message_id: &str, _emoji: &str) -> bool {
        false
    }

    /// Remove an emoji reaction previously added by the bot. The default refuses.
    async fn remove_reaction(&self, _chat_id: &str, _message_id: &str, _emoji: &str) -> bool {
        false
    }

    /// Whether [`send_poll`](Self::send_poll) is implemented for this platform.
    fn supports_polls(&self) -> bool {
        false
    }

    /// Send a poll. The default refuses: a platform without a poll API must not claim a poll.
    async fn send_poll(&self, _chat_id: &str, _question: &str, _options: &[String]) -> SendResult {
        SendResult::failed("this platform does not send polls", Some("bad_format"))
    }

    /// Whether [`send_card`](Self::send_card) is implemented for this platform.
    fn supports_cards(&self) -> bool {
        false
    }

    /// Send a provider-native rich card payload: Slack Block Kit `blocks`, Feishu `card`,
    /// Teams adaptive card, or Google Chat `cardsV2`. The JSON is passed through to the
    /// platform; the default refuses rather than dropping a card on the floor.
    async fn send_card(&self, _chat_id: &str, _card: &Value) -> SendResult {
        SendResult::failed("this platform does not send rich cards", Some("bad_format"))
    }

    /// Stream one partial frame into a draft message. `content` is the full accumulated text;
    /// reusing `draft_id` animates the same preview. The final message is sent by [`send`](Self::send).
    async fn send_draft(&self, _chat_id: &str, _draft_id: u64, _content: &str) -> SendResult {
        SendResult::failed(
            "this platform does not support draft streaming",
            Some("bad_format"),
        )
    }

    /// Send one attachment. The default is a truthful refusal: a platform that has not
    /// implemented uploads must not pretend the media was delivered.
    async fn send_media(
        &self,
        _chat_id: &str,
        _media: &OutboundMedia,
        _reply_to: Option<&str>,
    ) -> SendResult {
        SendResult::failed("this platform does not send media", Some("bad_format"))
    }

    // ----- capability flags (hermes: class attributes; defaults match base.py) -----

    fn supports_code_blocks(&self) -> bool {
        false
    }
    /// Renders status text in the typing indicator; pair with [`set_status_text`](Self::set_status_text).
    fn supports_status_text(&self) -> bool {
        false
    }
    /// Adapter can wake a fresh turn after the current one ends (hermes feeds this
    /// into `HERMES_SESSION_ASYNC_DELIVERY`).
    fn supports_async_delivery(&self) -> bool {
        true
    }
    /// `send` chunks natively (hermes: `truncate_message`), so the router must NOT truncate.
    fn splits_long_messages(&self) -> bool {
        false
    }
    /// Command prefix always typeable on this platform.
    fn typed_command_prefix(&self) -> &str {
        "/"
    }
    /// Cron flat-reply surface; `false` fails SAFE (the reply goes to a thread).
    fn supports_inchannel_continuable(&self) -> bool {
        false
    }
    /// A human can answer the "session restored" prompt; webhook adapters set `false`.
    fn interactive_resume(&self) -> bool {
        true
    }
    /// Some platforms (DingTalk AI Cards) only consider a message final on `edit_message` with `finalize`.
    fn requires_edit_finalize(&self) -> bool {
        false
    }
    /// Native streaming preview (`send_draft`); otherwise the stream consumer edits.
    fn supports_draft_streaming(&self) -> bool {
        false
    }

    // ----- limits / length measurement -----

    fn max_message_length(&self) -> usize {
        4096
    }
    /// hermes: `message_len_fn`; default is code points (Python `len`), Telegram overrides
    /// with [`utf16_len`].
    fn message_length(&self, text: &str) -> usize {
        text.chars().count()
    }

    // ----- default (non-abstract) surface -----

    /// Render content for this platform; default is identity (e.g. Telegram rewrites MarkdownV2).
    fn format_message(&self, content: &str) -> String {
        content.to_string()
    }

    /// Per-source toolset override REPLACING platform toolsets, or None (default).
    fn toolsets_for_source(&self, _source: &SessionSource) -> Option<Vec<String>> {
        None
    }

    /// No-op unless the platform renders status text; in-memory only.
    fn set_status_text(&self, _chat_id: &str, _text: Option<&str>) {}

    /// Typing indicator; no-op default.
    async fn send_typing(&self, _chat_id: &str, _metadata: Option<Value>) {}

    async fn stop_typing(&self, _chat_id: &str) {}

    /// Default image behavior: send the URL as text. Platforms with native media override this.
    async fn send_image(
        &self,
        chat_id: &str,
        url: &str,
        caption: Option<&str>,
        _metadata: Option<Value>,
    ) -> SendResult {
        let text = match caption {
            Some(caption) => format!("{caption}\n{url}"),
            None => url.to_string(),
        };
        self.send(chat_id, &text, None, None).await
    }

    /// Default: report that editing is not supported; callers fall back to a fresh send.
    async fn edit_message(
        &self,
        _chat_id: &str,
        _message_id: &str,
        _content: &str,
        _finalize: bool,
    ) -> SendResult {
        SendResult::failed(
            "edit_message is not supported on this platform",
            Some("bad_format"),
        )
    }

    /// Best effort; default forgets the message.
    async fn delete_message(&self, _chat_id: &str, _message_id: &str) {}

    /// Fresh thread for handoff (cron / detached sessions), or None.
    async fn create_handoff_thread(&self, _parent_chat_id: &str, _name: &str) -> Option<String> {
        None
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    struct Dummy;

    #[async_trait]
    impl PlatformAdapter for Dummy {
        async fn connect(&mut self, _is_reconnect: bool) -> bool {
            true
        }
        async fn disconnect(&mut self) {}
        async fn send(
            &self,
            chat_id: &str,
            content: &str,
            _reply_to: Option<&str>,
            _metadata: Option<Value>,
        ) -> SendResult {
            SendResult::ok(format!("{chat_id}:{content}"))
        }
        async fn get_chat_info(&self, chat_id: &str) -> ChatInfo {
            ChatInfo {
                name: chat_id.to_string(),
                chat_type: "group".to_string(),
            }
        }
    }

    #[test]
    fn utf16_len_counts_units_not_codepoints() {
        assert_eq!(utf16_len("abc"), 3);
        assert_eq!("😀".chars().count(), 1);
        assert_eq!(utf16_len("😀"), 2);
        assert_eq!(utf16_len("a😀b"), 4);
    }

    #[test]
    fn ok_and_failed_results_shape() {
        let ok = SendResult::ok("m1");
        assert!(ok.success);
        assert_eq!(ok.message_id.as_deref(), Some("m1"));
        assert!(ok.error_kind.is_none());
        assert!(!ok.retryable);

        let failed = SendResult::failed("nope", Some("forbidden"));
        assert!(!failed.success);
        assert_eq!(failed.error.as_deref(), Some("nope"));
        assert_eq!(failed.error_kind.as_deref(), Some("forbidden"));
    }

    #[test]
    fn partial_overflow_round_trips() {
        let result = SendResult::ok("m9").with_partial_overflow(2, 3);
        assert_eq!(result.partial_overflow(), Some((2, 3)));
        assert_eq!(SendResult::ok("m1").partial_overflow(), None);
    }

    #[test]
    fn continuation_ids_keep_last_as_message_id() {
        let result = SendResult::ok_split("last", vec!["a".into(), "b".into(), "last".into()]);
        assert_eq!(result.message_id.as_deref(), Some("last"));
        assert_eq!(result.continuation_message_ids, vec!["a", "b", "last"]);
    }

    #[test]
    fn turn_reply_ephemerals_keep_ttl() {
        assert_eq!(TurnReply::text("hi").body(), "hi");
        assert_eq!(TurnReply::text("hi").ephemeral_ttl(), None);
        assert_eq!(
            TurnReply::ephemeral("gone soon", Some(30)).ephemeral_ttl(),
            Some(Some(30))
        );
        assert_eq!(
            TurnReply::ephemeral("gone forever", Some(0)).ephemeral_ttl(),
            Some(Some(0))
        );
    }

    #[tokio::test]
    async fn card_and_poll_defaults_refuse_honestly() {
        let dummy = Dummy;
        assert!(!dummy.supports_cards());
        assert!(!dummy.supports_polls());
        let card = serde_json::json!({"type": "AdaptiveCard"});
        assert!(!dummy.send_card("chat", &card).await.success);
        assert!(
            !dummy
                .send_poll("chat", "question?", &["a".into(), "b".into()])
                .await
                .success
        );
    }

    #[tokio::test]
    async fn reaction_defaults_refuse_honestly() {
        let dummy = Dummy;
        assert!(!dummy.supports_reactions());
        assert!(!dummy.add_reaction("chat", "msg", "👍").await);
        assert!(!dummy.remove_reaction("chat", "msg", "👍").await);
    }

    #[tokio::test]
    async fn media_defaults_refuse_honestly() {
        let dummy = Dummy;
        assert!(!dummy.supports_media());
        let media = OutboundMedia::image_url("https://example.test/a.png");
        let result = dummy.send_media("chat", &media, None).await;
        assert!(!result.success);
        assert_eq!(result.error_kind.as_deref(), Some("bad_format"));
    }

    #[tokio::test]
    async fn default_capability_flags_match_hermes() {
        let dummy = Dummy;
        assert!(!dummy.supports_code_blocks());
        assert!(!dummy.supports_status_text());
        assert!(dummy.supports_async_delivery());
        assert!(!dummy.splits_long_messages());
        assert_eq!(dummy.typed_command_prefix(), "/");
        assert!(!dummy.supports_inchannel_continuable());
        assert!(dummy.interactive_resume());
        assert!(!dummy.requires_edit_finalize());
        assert!(!dummy.supports_draft_streaming());
        assert_eq!(dummy.max_message_length(), 4096);
        assert_eq!(dummy.message_length("héllo"), 5);
    }

    #[tokio::test]
    async fn dummy_adapter_smoke_test() {
        let mut dummy = Dummy;
        assert!(dummy.connect(false).await);
        let result = dummy.send("c1", "hello", None, None).await;
        assert!(result.success);
        let info = dummy.get_chat_info("c1").await;
        assert_eq!(info.chat_type, "group");
        dummy.disconnect().await;
    }
}
