//! The gateway host loop: one platform in, one agent turn per event, one reply out.
//!
//! Adapters know their wire format but nothing about the runtime. This module is the seam between
//! them: a [`GatewayPlatform`] exposes *next events* and *send*, and [`serve`] drives the loop. The
//! host supplies the handler — in VIKRAM that is a closure that submits the event to a runtime
//! session and returns the turn's final text.
//!
//! Wrappers for the polling adapters live here too, so a host does not have to know each adapter's
//! cursor shape.

use std::future::Future;
use std::pin::Pin;
use std::sync::{Arc, Mutex};

use async_trait::async_trait;
use tokio::sync::watch;

use crate::event::MessageEvent;
use crate::platform::{OutboundMedia, PlatformAdapter, SendResult};
use crate::platforms::{
    a2a::A2aEngine,
    buzz::BuzzAdapter,
    dingtalk_stream::DingTalkStream,
    discord::{DiscordAdapter, DiscordGateway},
    email::EmailAdapter,
    irc::IrcAdapter,
    matrix::MatrixAdapter,
    mattermost::MattermostAdapter,
    ntfy::NtfyAdapter,
    photon::{PhotonAdapter, SseSource},
    signal::SignalAdapter,
    simplex::SimplexAdapter,
    telegram::TelegramAdapter,
    wecom::WeComAdapter,
    weixin::WeixinAdapter,
    whatsapp::WhatsAppAdapter,
    yuanbao::YuanbaoAdapter,
};

/// What the host returns for one event: `Some(text)` is sent back, `None` means the handler already
/// delivered (or deliberately stayed silent).
pub type ReplyFuture = Pin<Box<dyn Future<Output = Option<String>> + Send>>;
pub type ReplyHandler = Arc<dyn Fn(MessageEvent) -> ReplyFuture + Send + Sync>;

#[async_trait]
pub trait GatewayPlatform: Send + Sync {
    fn name(&self) -> &str;
    /// Block until at least one event is available, or return an error to trigger a backoff.
    async fn next_events(&self) -> Result<Vec<MessageEvent>, String>;
    async fn send(&self, chat_id: &str, content: &str) -> SendResult;

    /// Send one attachment. The default refuses truthfully; adapters that implement uploads
    /// override it.
    async fn send_media(
        &self,
        _chat_id: &str,
        _media: &OutboundMedia,
        _reply_to: Option<&str>,
    ) -> SendResult {
        SendResult::failed("this platform does not send media", Some("bad_format"))
    }
}

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct HostStats {
    pub events: u64,
    pub replies: u64,
    pub errors: u64,
}

/// Serve one platform until `stop` flips to `true`.
///
/// A read error is not fatal: it is counted and retried after a short pause, because a dropped
/// socket is the normal state of a messaging platform, not an outage of this process.
pub async fn serve(
    platform: Arc<dyn GatewayPlatform>,
    handler: ReplyHandler,
    mut stop: watch::Receiver<bool>,
) -> HostStats {
    let mut stats = HostStats::default();
    loop {
        if *stop.borrow() {
            return stats;
        }
        let events = match platform.next_events().await {
            Ok(events) => events,
            Err(err) => {
                stats.errors += 1;
                eprintln!("[gateway:{}] read failed: {err}", platform.name());
                if wait_or_stop(&mut stop, std::time::Duration::from_secs(1)).await {
                    return stats;
                }
                continue;
            }
        };
        // A platform that answers "nothing yet" immediately would otherwise spin; a short pause
        // is enough because real long-poll transports already block server-side.
        if events.is_empty() && wait_or_stop(&mut stop, std::time::Duration::from_millis(250)).await
        {
            return stats;
        }
        for event in events {
            if *stop.borrow() {
                return stats;
            }
            let chat_id = event.source.chat_id.clone();
            stats.events += 1;
            let reply = (handler)(event).await;
            if let Some(text) = reply {
                if text.trim().is_empty() {
                    continue;
                }
                let result = platform.send(&chat_id, &text).await;
                if result.success {
                    stats.replies += 1;
                } else {
                    stats.errors += 1;
                    eprintln!(
                        "[gateway:{}] reply to {chat_id} failed: {}",
                        platform.name(),
                        result.error.unwrap_or_else(|| "unknown error".into())
                    );
                }
            }
        }
    }
}

async fn wait_or_stop(stop: &mut watch::Receiver<bool>, duration: std::time::Duration) -> bool {
    tokio::select! {
        _ = tokio::time::sleep(duration) => false,
        changed = stop.changed() => changed.is_err() || *stop.borrow(),
    }
}

// ---------------------------------------------------------------------------------------------
// Adapter wrappers. Each owns the cursor the adapter's poll method needs.
// ---------------------------------------------------------------------------------------------

pub struct TelegramPlatform {
    adapter: TelegramAdapter,
    cursor: Mutex<Option<i64>>,
}

impl TelegramPlatform {
    pub fn new(adapter: TelegramAdapter) -> Self {
        Self {
            adapter,
            cursor: Mutex::new(None),
        }
    }
}

#[async_trait]
impl GatewayPlatform for TelegramPlatform {
    fn name(&self) -> &str {
        "telegram"
    }

    async fn next_events(&self) -> Result<Vec<MessageEvent>, String> {
        let offset = *self.cursor.lock().unwrap_or_else(|err| err.into_inner());
        let updates = self.adapter.poll_once(offset, 25).await?;
        let mut events = Vec::with_capacity(updates.len());
        let mut next = offset;
        for update in updates {
            next = Some(match next {
                Some(current) => current.max(update.update_id + 1),
                None => update.update_id + 1,
            });
            events.push(update.event);
        }
        if next != offset {
            *self.cursor.lock().unwrap_or_else(|err| err.into_inner()) = next;
        }
        Ok(events)
    }

    async fn send(&self, chat_id: &str, content: &str) -> SendResult {
        self.adapter.send(chat_id, content, None, None).await
    }

    async fn send_media(
        &self,
        chat_id: &str,
        media: &OutboundMedia,
        reply_to: Option<&str>,
    ) -> SendResult {
        self.adapter.send_media(chat_id, media, reply_to).await
    }
}

pub struct SignalPlatform {
    adapter: SignalAdapter,
}

impl SignalPlatform {
    pub fn new(adapter: SignalAdapter) -> Self {
        Self { adapter }
    }
}

#[async_trait]
impl GatewayPlatform for SignalPlatform {
    fn name(&self) -> &str {
        "signal"
    }

    async fn next_events(&self) -> Result<Vec<MessageEvent>, String> {
        self.adapter.poll_once(20).await
    }

    async fn send(&self, chat_id: &str, content: &str) -> SendResult {
        self.adapter.send(chat_id, content, None, None).await
    }

    async fn send_media(
        &self,
        chat_id: &str,
        media: &OutboundMedia,
        reply_to: Option<&str>,
    ) -> SendResult {
        self.adapter.send_media(chat_id, media, reply_to).await
    }
}

pub struct NtfyPlatform {
    adapter: NtfyAdapter,
    cursor: Mutex<Option<String>>,
}

impl NtfyPlatform {
    pub fn new(adapter: NtfyAdapter) -> Self {
        Self {
            adapter,
            cursor: Mutex::new(None),
        }
    }
}

#[async_trait]
impl GatewayPlatform for NtfyPlatform {
    fn name(&self) -> &str {
        "ntfy"
    }

    async fn next_events(&self) -> Result<Vec<MessageEvent>, String> {
        let since = self
            .cursor
            .lock()
            .unwrap_or_else(|err| err.into_inner())
            .clone();
        let events = self.adapter.poll_once(since.as_deref()).await?;
        if let Some(last) = events.last()
            && let Some(id) = last.message_id.clone()
        {
            *self.cursor.lock().unwrap_or_else(|err| err.into_inner()) = Some(id);
        }
        tokio::time::sleep(std::time::Duration::from_secs(1)).await;
        Ok(events)
    }

    async fn send(&self, chat_id: &str, content: &str) -> SendResult {
        self.adapter.send(chat_id, content, None, None).await
    }
}

pub struct WeixinPlatform {
    adapter: WeixinAdapter,
    cursor: Mutex<Option<String>>,
}

impl WeixinPlatform {
    pub fn new(adapter: WeixinAdapter) -> Self {
        Self {
            adapter,
            cursor: Mutex::new(None),
        }
    }
}

#[async_trait]
impl GatewayPlatform for WeixinPlatform {
    fn name(&self) -> &str {
        "weixin"
    }

    async fn next_events(&self) -> Result<Vec<MessageEvent>, String> {
        let since = self
            .cursor
            .lock()
            .unwrap_or_else(|err| err.into_inner())
            .clone();
        let (events, next) = self.adapter.poll_once(since.as_deref()).await?;
        *self.cursor.lock().unwrap_or_else(|err| err.into_inner()) = Some(next);
        Ok(events)
    }

    async fn send(&self, chat_id: &str, content: &str) -> SendResult {
        self.adapter.send(chat_id, content, None, None).await
    }

    async fn send_media(
        &self,
        chat_id: &str,
        media: &OutboundMedia,
        reply_to: Option<&str>,
    ) -> SendResult {
        self.adapter.send_media(chat_id, media, reply_to).await
    }
}

pub struct SimplexPlatform {
    adapter: SimplexAdapter,
}

impl SimplexPlatform {
    pub fn new(adapter: SimplexAdapter) -> Self {
        Self { adapter }
    }
}

#[async_trait]
impl GatewayPlatform for SimplexPlatform {
    fn name(&self) -> &str {
        "simplex"
    }

    async fn next_events(&self) -> Result<Vec<MessageEvent>, String> {
        Ok(self.adapter.next_event().await?.into_iter().collect())
    }

    async fn send(&self, chat_id: &str, content: &str) -> SendResult {
        self.adapter.send(chat_id, content, None, None).await
    }
}

pub struct IrcPlatform {
    adapter: IrcAdapter,
}

impl IrcPlatform {
    pub fn new(adapter: IrcAdapter) -> Self {
        Self { adapter }
    }
}

#[async_trait]
impl GatewayPlatform for IrcPlatform {
    fn name(&self) -> &str {
        "irc"
    }

    async fn next_events(&self) -> Result<Vec<MessageEvent>, String> {
        Ok(self.adapter.next_event().await?.into_iter().collect())
    }

    async fn send(&self, chat_id: &str, content: &str) -> SendResult {
        self.adapter.send(chat_id, content, None, None).await
    }
}

/// A platform whose inbound events are pushed by an ingress route rather than polled. Used for
/// wake-only bridges (raft) and anything else that answers HTTP but does not speak a pull protocol.
pub struct ChannelPlatform {
    name: &'static str,
    receiver: tokio::sync::Mutex<tokio::sync::mpsc::Receiver<MessageEvent>>,
    refusal: &'static str,
}

impl ChannelPlatform {
    pub fn new(
        name: &'static str,
        receiver: tokio::sync::mpsc::Receiver<MessageEvent>,
        refusal: &'static str,
    ) -> Self {
        Self {
            name,
            receiver: tokio::sync::Mutex::new(receiver),
            refusal,
        }
    }
}

#[async_trait]
impl GatewayPlatform for ChannelPlatform {
    fn name(&self) -> &str {
        self.name
    }

    async fn next_events(&self) -> Result<Vec<MessageEvent>, String> {
        let mut receiver = self.receiver.lock().await;
        match receiver.recv().await {
            Some(event) => Ok(vec![event]),
            None => Err("event channel closed".to_string()),
        }
    }

    async fn send(&self, _chat_id: &str, _content: &str) -> SendResult {
        SendResult::failed(self.refusal, None)
    }
}

pub struct YuanbaoPlatform {
    adapter: YuanbaoAdapter,
}

impl YuanbaoPlatform {
    pub fn new(adapter: YuanbaoAdapter) -> Self {
        Self { adapter }
    }
}

#[async_trait]
impl GatewayPlatform for YuanbaoPlatform {
    fn name(&self) -> &str {
        "yuanbao"
    }

    async fn next_events(&self) -> Result<Vec<MessageEvent>, String> {
        Ok(self
            .adapter
            .next_event()
            .await
            .map_err(|err| err.to_string())?
            .into_iter()
            .collect())
    }

    async fn send(&self, chat_id: &str, content: &str) -> SendResult {
        self.adapter.send(chat_id, content, None, None).await
    }

    async fn send_media(
        &self,
        chat_id: &str,
        media: &OutboundMedia,
        reply_to: Option<&str>,
    ) -> SendResult {
        self.adapter.send_media(chat_id, media, reply_to).await
    }
}

pub struct BuzzPlatform {
    adapter: BuzzAdapter,
}

impl BuzzPlatform {
    pub fn new(adapter: BuzzAdapter) -> Self {
        Self { adapter }
    }
}

#[async_trait]
impl GatewayPlatform for BuzzPlatform {
    fn name(&self) -> &str {
        "buzz"
    }

    async fn next_events(&self) -> Result<Vec<MessageEvent>, String> {
        Ok(self
            .adapter
            .next_event()
            .await
            .map_err(|err| err.to_string())?
            .into_iter()
            .collect())
    }

    async fn send(&self, chat_id: &str, content: &str) -> SendResult {
        self.adapter.send(chat_id, content, None, None).await
    }
}

/// DingTalk replies go to the per-message `sessionWebhook` carried by the inbound callback, so the
/// host remembers the newest webhook per conversation. An expired or unknown webhook fails the
/// send truthfully; nothing is reported delivered.
pub struct DingTalkStreamPlatform {
    stream: DingTalkStream,
    webhooks: std::sync::Mutex<std::collections::BTreeMap<String, String>>,
}

impl DingTalkStreamPlatform {
    pub fn new(stream: DingTalkStream) -> Self {
        Self {
            stream,
            webhooks: std::sync::Mutex::new(std::collections::BTreeMap::new()),
        }
    }
}

#[async_trait]
impl GatewayPlatform for DingTalkStreamPlatform {
    fn name(&self) -> &str {
        "dingtalk"
    }

    async fn next_events(&self) -> Result<Vec<MessageEvent>, String> {
        let Some(event) = self
            .stream
            .next_event()
            .await
            .map_err(|err| err.to_string())?
        else {
            return Ok(Vec::new());
        };
        if let Some(webhook) = event
            .metadata
            .get("session_webhook")
            .and_then(|value| value.as_str())
            && let Ok(mut webhooks) = self.webhooks.lock()
        {
            webhooks.insert(event.source.chat_id.clone(), webhook.to_string());
        }
        Ok(vec![event])
    }

    async fn send(&self, chat_id: &str, content: &str) -> SendResult {
        let webhook = self
            .webhooks
            .lock()
            .ok()
            .and_then(|webhooks| webhooks.get(chat_id).cloned());
        match webhook {
            Some(webhook) => self.stream.reply(&webhook, content).await,
            None => SendResult::failed(
                format!("no DingTalk session webhook for {chat_id}"),
                Some("unknown"),
            ),
        }
    }
}

pub struct PhotonPlatform {
    adapter: PhotonAdapter,
    source: Arc<dyn SseSource>,
}

impl PhotonPlatform {
    /// The sidecar stream is injected, so the platform is testable without a sidecar process.
    pub fn new(adapter: PhotonAdapter, source: Arc<dyn SseSource>) -> Self {
        Self { adapter, source }
    }
}

#[async_trait]
impl GatewayPlatform for PhotonPlatform {
    fn name(&self) -> &str {
        "photon"
    }

    async fn next_events(&self) -> Result<Vec<MessageEvent>, String> {
        Ok(self
            .adapter
            .next_event(self.source.as_ref())
            .await?
            .into_iter()
            .collect())
    }

    async fn send(&self, chat_id: &str, content: &str) -> SendResult {
        self.adapter.send(chat_id, content, None, None).await
    }

    async fn send_media(
        &self,
        chat_id: &str,
        media: &OutboundMedia,
        reply_to: Option<&str>,
    ) -> SendResult {
        self.adapter.send_media(chat_id, media, reply_to).await
    }
}

pub struct EmailPlatform {
    adapter: EmailAdapter,
}

impl EmailPlatform {
    pub fn new(adapter: EmailAdapter) -> Self {
        Self { adapter }
    }
}

#[async_trait]
impl GatewayPlatform for EmailPlatform {
    fn name(&self) -> &str {
        "email"
    }

    async fn next_events(&self) -> Result<Vec<MessageEvent>, String> {
        self.adapter.poll_once().await
    }

    async fn send(&self, chat_id: &str, content: &str) -> SendResult {
        self.adapter.send(chat_id, content, None, None).await
    }

    async fn send_media(
        &self,
        chat_id: &str,
        media: &OutboundMedia,
        reply_to: Option<&str>,
    ) -> SendResult {
        self.adapter.send_media(chat_id, media, reply_to).await
    }
}

pub struct A2aPlatform {
    engine: Arc<A2aEngine>,
    sender: tokio::sync::mpsc::Sender<MessageEvent>,
    receiver: tokio::sync::Mutex<tokio::sync::mpsc::Receiver<MessageEvent>>,
    /// contextId → task id, so a reply can complete the task the HTTP request created.
    tasks: Mutex<std::collections::HashMap<String, String>>,
}

impl A2aPlatform {
    pub fn new(engine: Arc<A2aEngine>) -> Arc<Self> {
        let (sender, receiver) = tokio::sync::mpsc::channel(64);
        Arc::new(Self {
            engine,
            sender,
            receiver: tokio::sync::Mutex::new(receiver),
            tasks: Mutex::new(std::collections::HashMap::new()),
        })
    }

    /// The ingress route's half: every accepted A2A message becomes one turn.
    pub fn sender(&self) -> tokio::sync::mpsc::Sender<MessageEvent> {
        self.sender.clone()
    }

    /// Bind an A2A context to the task the engine created for it.
    pub fn remember(&self, context_id: &str, task_id: &str) {
        if let Ok(mut tasks) = self.tasks.lock() {
            tasks.insert(context_id.to_string(), task_id.to_string());
        }
    }
}

#[async_trait]
impl GatewayPlatform for A2aPlatform {
    fn name(&self) -> &str {
        "a2a"
    }

    async fn next_events(&self) -> Result<Vec<MessageEvent>, String> {
        let mut receiver = self.receiver.lock().await;
        match receiver.recv().await {
            Some(event) => Ok(vec![event]),
            None => Err("a2a channel closed".to_string()),
        }
    }

    async fn send(&self, chat_id: &str, content: &str) -> SendResult {
        let task_id = self
            .tasks
            .lock()
            .ok()
            .and_then(|tasks| tasks.get(chat_id).cloned());
        let Some(task_id) = task_id else {
            return SendResult::failed("no A2A task for this context", Some("not_found"));
        };
        match self.engine.complete(&task_id, content) {
            Some(_) => SendResult::ok(task_id),
            None => SendResult::failed("A2A task is no longer completable", Some("not_found")),
        }
    }
}

pub struct DiscordPlatform {
    adapter: DiscordAdapter,
    gateway: DiscordGateway,
}

impl DiscordPlatform {
    /// REST for sending, gateway socket for receiving.
    pub fn new(adapter: DiscordAdapter, gateway: DiscordGateway) -> Self {
        Self { adapter, gateway }
    }
}

#[async_trait]
impl GatewayPlatform for DiscordPlatform {
    fn name(&self) -> &str {
        "discord"
    }

    async fn next_events(&self) -> Result<Vec<MessageEvent>, String> {
        Ok(self.gateway.next_event().await?.into_iter().collect())
    }

    async fn send(&self, chat_id: &str, content: &str) -> SendResult {
        self.adapter.send(chat_id, content, None, None).await
    }

    async fn send_media(
        &self,
        chat_id: &str,
        media: &OutboundMedia,
        reply_to: Option<&str>,
    ) -> SendResult {
        self.adapter.send_media(chat_id, media, reply_to).await
    }
}

pub struct MattermostPlatform {
    adapter: MattermostAdapter,
    channel_id: String,
    cursor: Mutex<u64>,
}

impl MattermostPlatform {
    /// Mattermost's REST poll is per channel, so the runner must be told which one to watch.
    pub fn new(adapter: MattermostAdapter, channel_id: impl Into<String>) -> Self {
        Self {
            adapter,
            channel_id: channel_id.into(),
            cursor: Mutex::new(0),
        }
    }
}

#[async_trait]
impl GatewayPlatform for MattermostPlatform {
    fn name(&self) -> &str {
        "mattermost"
    }

    async fn next_events(&self) -> Result<Vec<MessageEvent>, String> {
        let since = *self.cursor.lock().unwrap_or_else(|err| err.into_inner());
        let events = self.adapter.poll_once(&self.channel_id, since).await?;
        let next = events
            .iter()
            .filter_map(|event| event.platform_update_id)
            .max()
            .map(|stamp| stamp as u64)
            .unwrap_or(since);
        if next != since {
            *self.cursor.lock().unwrap_or_else(|err| err.into_inner()) = next + 1;
        }
        Ok(events)
    }

    async fn send(&self, chat_id: &str, content: &str) -> SendResult {
        self.adapter.send(chat_id, content, None, None).await
    }

    async fn send_media(
        &self,
        chat_id: &str,
        media: &OutboundMedia,
        reply_to: Option<&str>,
    ) -> SendResult {
        self.adapter.send_media(chat_id, media, reply_to).await
    }
}

pub struct MatrixPlatform {
    adapter: MatrixAdapter,
    cursor: Mutex<Option<String>>,
}

impl MatrixPlatform {
    pub fn new(adapter: MatrixAdapter) -> Self {
        Self {
            adapter,
            cursor: Mutex::new(None),
        }
    }
}

#[async_trait]
impl GatewayPlatform for MatrixPlatform {
    fn name(&self) -> &str {
        "matrix"
    }

    async fn next_events(&self) -> Result<Vec<MessageEvent>, String> {
        let since = self
            .cursor
            .lock()
            .unwrap_or_else(|err| err.into_inner())
            .clone();
        let (events, next) = self.adapter.poll_once(since.as_deref(), 25_000).await?;
        if let Some(next) = next {
            *self.cursor.lock().unwrap_or_else(|err| err.into_inner()) = Some(next);
        }
        Ok(events)
    }

    async fn send(&self, chat_id: &str, content: &str) -> SendResult {
        self.adapter.send(chat_id, content, None, None).await
    }

    async fn send_media(
        &self,
        chat_id: &str,
        media: &OutboundMedia,
        reply_to: Option<&str>,
    ) -> SendResult {
        self.adapter.send_media(chat_id, media, reply_to).await
    }
}

pub struct WhatsAppPlatform {
    adapter: WhatsAppAdapter,
}

impl WhatsAppPlatform {
    pub fn new(adapter: WhatsAppAdapter) -> Self {
        Self { adapter }
    }
}

#[async_trait]
impl GatewayPlatform for WhatsAppPlatform {
    fn name(&self) -> &str {
        "whatsapp"
    }

    async fn next_events(&self) -> Result<Vec<MessageEvent>, String> {
        self.adapter.poll_once().await
    }

    async fn send(&self, chat_id: &str, content: &str) -> SendResult {
        self.adapter.send(chat_id, content, None, None).await
    }

    async fn send_media(
        &self,
        chat_id: &str,
        media: &OutboundMedia,
        reply_to: Option<&str>,
    ) -> SendResult {
        self.adapter.send_media(chat_id, media, reply_to).await
    }
}

pub struct WeComPlatform {
    adapter: WeComAdapter,
}

impl WeComPlatform {
    pub fn new(adapter: WeComAdapter) -> Self {
        Self { adapter }
    }
}

#[async_trait]
impl GatewayPlatform for WeComPlatform {
    fn name(&self) -> &str {
        "wecom"
    }

    async fn next_events(&self) -> Result<Vec<MessageEvent>, String> {
        Ok(self.adapter.next_event().await?.into_iter().collect())
    }

    async fn send(&self, chat_id: &str, content: &str) -> SendResult {
        self.adapter.send(chat_id, content, None, None).await
    }

    async fn send_media(
        &self,
        chat_id: &str,
        media: &OutboundMedia,
        reply_to: Option<&str>,
    ) -> SendResult {
        self.adapter.send_media(chat_id, media, reply_to).await
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use tokio::sync::Notify;

    struct FakePlatform {
        events: Mutex<Vec<MessageEvent>>,
        sent: Mutex<Vec<(String, String)>>,
        wake: Notify,
    }

    impl FakePlatform {
        fn new(events: Vec<MessageEvent>) -> Arc<Self> {
            Arc::new(Self {
                events: Mutex::new(events),
                sent: Mutex::new(Vec::new()),
                wake: Notify::new(),
            })
        }
    }

    #[async_trait]
    impl GatewayPlatform for FakePlatform {
        fn name(&self) -> &str {
            "fake"
        }

        async fn next_events(&self) -> Result<Vec<MessageEvent>, String> {
            if self.events.lock().unwrap().is_empty() {
                self.wake.notified().await;
            }
            Ok(self.events.lock().unwrap().drain(..).collect())
        }

        async fn send(&self, chat_id: &str, content: &str) -> SendResult {
            self.sent
                .lock()
                .unwrap()
                .push((chat_id.to_string(), content.to_string()));
            SendResult::ok("m")
        }
    }

    fn event(chat: &str, text: &str) -> MessageEvent {
        let platform = crate::event::Platform::new("fake").expect("valid");
        MessageEvent::new(text, crate::event::SessionSource::new(platform, chat))
    }

    fn echo() -> ReplyHandler {
        Arc::new(|event: MessageEvent| {
            let reply = format!("echo: {}", event.text);
            Box::pin(async move { Some(reply) })
        })
    }

    #[tokio::test]
    async fn one_event_produces_one_reply() {
        let platform = FakePlatform::new(vec![event("chat-1", "hello")]);
        let (stop_tx, stop_rx) = watch::channel(false);
        let task = tokio::spawn(serve(platform.clone(), echo(), stop_rx));
        // Wait until the reply is recorded, then stop.
        let deadline = tokio::time::Instant::now() + std::time::Duration::from_secs(2);
        loop {
            if !platform.sent.lock().unwrap().is_empty() {
                break;
            }
            assert!(tokio::time::Instant::now() < deadline, "no reply");
            tokio::time::sleep(std::time::Duration::from_millis(10)).await;
        }
        stop_tx.send(true).unwrap();
        platform.wake.notify_waiters();
        let stats = task.await.unwrap();
        assert_eq!(stats.events, 1);
        assert_eq!(stats.replies, 1);
        assert_eq!(
            platform.sent.lock().unwrap().as_slice(),
            &[("chat-1".to_string(), "echo: hello".to_string())]
        );
    }

    #[tokio::test]
    async fn a_silent_handler_sends_nothing() {
        let platform = FakePlatform::new(vec![event("c", "x")]);
        let (stop_tx, stop_rx) = watch::channel(false);
        let handler: ReplyHandler = Arc::new(|_| Box::pin(async { None }));
        let task = tokio::spawn(serve(platform.clone(), handler, stop_rx));
        let deadline = tokio::time::Instant::now() + std::time::Duration::from_secs(2);
        loop {
            // The event is consumed when the queue empties; then stop.
            if platform.events.lock().unwrap().is_empty() {
                break;
            }
            assert!(tokio::time::Instant::now() < deadline);
            tokio::time::sleep(std::time::Duration::from_millis(10)).await;
        }
        tokio::time::sleep(std::time::Duration::from_millis(50)).await;
        stop_tx.send(true).unwrap();
        platform.wake.notify_waiters();
        let stats = task.await.unwrap();
        assert_eq!(stats.events, 1);
        assert_eq!(stats.replies, 0);
        assert!(platform.sent.lock().unwrap().is_empty());
    }

    #[tokio::test]
    async fn stop_before_any_event_ends_the_loop() {
        let platform = FakePlatform::new(vec![]);
        let (_stop_tx, stop_rx) = watch::channel(true);
        let stats = serve(platform, echo(), stop_rx).await;
        assert_eq!(stats, HostStats::default());
    }

    #[test]
    fn stats_are_comparable() {
        assert_eq!(
            HostStats {
                events: 1,
                replies: 1,
                errors: 0
            },
            HostStats {
                events: 1,
                replies: 1,
                errors: 0
            }
        );
    }
}
