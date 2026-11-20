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

use crate::delivery::{DeliveryLedger, NewDelivery, now_unix};
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

/// How often the delivery-ledger sweep runs: reclaim stale claims, then send due rows.
const LEDGER_SWEEP_INTERVAL: std::time::Duration = std::time::Duration::from_secs(2);
/// Rows one sweep may claim.
const LEDGER_SWEEP_BATCH: usize = 16;

/// Serve one platform until `stop` flips to `true`.
///
/// Replies go through an in-memory [`DeliveryLedger`]: a failed send is recorded and retried by
/// the sweep instead of being dropped. [`serve_with_ledger`] is the entry point for a caller that
/// can supply a file-backed ledger (the documented `gateway_state.db`).
pub async fn serve(
    platform: Arc<dyn GatewayPlatform>,
    handler: ReplyHandler,
    stop: watch::Receiver<bool>,
) -> HostStats {
    let ledger = match DeliveryLedger::in_memory() {
        Ok(ledger) => Some(Arc::new(ledger)),
        Err(err) => {
            eprintln!(
                "[gateway] in-memory delivery ledger unavailable: {err}; replies are sent without retry"
            );
            None
        }
    };
    serve_with_ledger(platform, handler, stop, ledger).await
}

/// [`serve`] with a caller-supplied ledger. `None` disables the ledger and keeps the legacy
/// send-and-forget path.
pub async fn serve_with_ledger(
    platform: Arc<dyn GatewayPlatform>,
    handler: ReplyHandler,
    mut stop: watch::Receiver<bool>,
    ledger: Option<Arc<DeliveryLedger>>,
) -> HostStats {
    let sweep = ledger
        .as_ref()
        .map(|ledger| spawn_ledger_sweep(Arc::clone(&platform), Arc::clone(ledger), stop.clone()));
    let mut stats = HostStats::default();
    let mut reply_seq = 0u64;
    loop {
        if *stop.borrow() {
            break;
        }
        let events = match platform.next_events().await {
            Ok(events) => events,
            Err(err) => {
                stats.errors += 1;
                eprintln!("[gateway:{}] read failed: {err}", platform.name());
                if wait_or_stop(&mut stop, std::time::Duration::from_secs(1)).await {
                    break;
                }
                continue;
            }
        };
        // A platform that answers "nothing yet" immediately would otherwise spin; a short pause
        // is enough because real long-poll transports already block server-side.
        if events.is_empty() && wait_or_stop(&mut stop, std::time::Duration::from_millis(250)).await
        {
            break;
        }
        for event in events {
            if *stop.borrow() {
                break;
            }
            let chat_id = event.source.chat_id.clone();
            // The platform message id is the ledger identity when the adapter supplied one
            // (hermes uses it so a redelivered update cannot enqueue a second reply); without
            // one, a unique id per reply keeps distinct answers distinct.
            let ledger_id = event
                .ledger_message_id
                .clone()
                .or_else(|| event.message_id.clone())
                .unwrap_or_else(|| {
                    reply_seq += 1;
                    format!("{}:{chat_id}:{}:{reply_seq}", platform.name(), now_unix())
                });
            stats.events += 1;
            let reply = (handler)(event).await;
            if let Some(text) = reply {
                if text.trim().is_empty() {
                    continue;
                }
                deliver_text(
                    &platform,
                    ledger.as_deref(),
                    &ledger_id,
                    &chat_id,
                    &text,
                    &mut stats,
                )
                .await;
            }
        }
    }
    if let Some(task) = sweep {
        task.abort();
    }
    stats
}

/// Send one reply. With a ledger the send is claim-then-record so a failure becomes a retryable
/// row; without one (or when the ledger itself errors) the legacy direct send is preserved.
async fn deliver_text(
    platform: &Arc<dyn GatewayPlatform>,
    ledger: Option<&DeliveryLedger>,
    ledger_id: &str,
    chat_id: &str,
    text: &str,
    stats: &mut HostStats,
) {
    let Some(ledger) = ledger else {
        send_direct(platform, chat_id, text, stats).await;
        return;
    };
    let now = now_unix();
    let delivery = NewDelivery::new(ledger_id, platform.name(), chat_id, text);
    if let Err(err) = ledger.enqueue(delivery, now) {
        eprintln!(
            "[gateway:{}] delivery ledger enqueue failed: {err}; sending directly",
            platform.name()
        );
        send_direct(platform, chat_id, text, stats).await;
        return;
    }
    match ledger.claim(ledger_id, now) {
        Ok(Some(_)) => {
            let result = platform.send(chat_id, text).await;
            if let Err(err) = ledger.record_send_result(ledger_id, &result, now_unix()) {
                eprintln!(
                    "[gateway:{}] delivery ledger record failed for {ledger_id}: {err}",
                    platform.name()
                );
            }
            record_send_stats(stats, platform.name(), chat_id, &result);
        }
        // The row is already delivered, in flight, or waiting out flood control: the ledger (and
        // the sweep) owns it now, so a duplicate event must not send a second copy.
        Ok(None) => {}
        Err(err) => {
            eprintln!(
                "[gateway:{}] delivery ledger claim failed for {ledger_id}: {err}; sending directly",
                platform.name()
            );
            send_direct(platform, chat_id, text, stats).await;
        }
    }
}

async fn send_direct(
    platform: &Arc<dyn GatewayPlatform>,
    chat_id: &str,
    text: &str,
    stats: &mut HostStats,
) {
    let result = platform.send(chat_id, text).await;
    record_send_stats(stats, platform.name(), chat_id, &result);
}

fn record_send_stats(
    stats: &mut HostStats,
    platform_name: &str,
    chat_id: &str,
    result: &SendResult,
) {
    if result.success {
        stats.replies += 1;
    } else {
        stats.errors += 1;
        eprintln!(
            "[gateway:{platform_name}] reply to {chat_id} failed: {}",
            result
                .error
                .clone()
                .unwrap_or_else(|| "unknown error".into())
        );
    }
}

/// Background task owning retries: reclaim rows a dead send stranded, then claim and send every
/// due row through the same platform transport. Stops when `stop` flips or the task is aborted.
fn spawn_ledger_sweep(
    platform: Arc<dyn GatewayPlatform>,
    ledger: Arc<DeliveryLedger>,
    mut stop: watch::Receiver<bool>,
) -> tokio::task::JoinHandle<()> {
    tokio::spawn(async move {
        let mut interval = tokio::time::interval(LEDGER_SWEEP_INTERVAL);
        interval.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Delay);
        loop {
            tokio::select! {
                _ = interval.tick() => {
                    sweep_once(&platform, &ledger, now_unix()).await;
                }
                changed = stop.changed() => {
                    if changed.is_err() || *stop.borrow() {
                        break;
                    }
                }
            }
        }
    })
}

/// One sweep pass. `now` is explicit so tests are deterministic.
///
/// Resumed rows resend their whole body: `GatewayPlatform::send` takes the full text and the
/// chunk boundaries that produced a `partial_overflow` live inside the adapter, so the ledger's
/// delivered-chunk bookkeeping is recorded faithfully even though a tail-only resend is not yet
/// expressible at this layer.
async fn sweep_once(platform: &Arc<dyn GatewayPlatform>, ledger: &DeliveryLedger, now: i64) {
    if let Err(err) = ledger.reclaim_stale(now) {
        eprintln!("[gateway:{}] ledger reclaim failed: {err}", platform.name());
    }
    let claimed = match ledger.claim_due(now, LEDGER_SWEEP_BATCH) {
        Ok(claimed) => claimed,
        Err(err) => {
            eprintln!("[gateway:{}] ledger claim failed: {err}", platform.name());
            return;
        }
    };
    for entry in claimed {
        let result = platform.send(&entry.chat_id, &entry.body).await;
        if let Err(err) = ledger.record_send_result(&entry.ledger_id, &result, now_unix()) {
            eprintln!(
                "[gateway:{}] ledger record failed for {}: {err}",
                platform.name(),
                entry.ledger_id
            );
        }
        if !result.success {
            eprintln!(
                "[gateway:{}] ledger retry to {} failed: {}",
                platform.name(),
                entry.chat_id,
                result
                    .error
                    .clone()
                    .unwrap_or_else(|| "unknown error".into())
            );
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
            // The map is a cache of the newest webhook per chat — bounded so a long-running
            // gateway can't grow it forever. An evicted chat simply re-registers on its next
            // inbound message.
            const MAX_WEBHOOKS: usize = 4096;
            if webhooks.len() >= MAX_WEBHOOKS {
                webhooks.pop_first();
            }
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
            // Bounded like the DingTalk webhook cache: an evicted context simply gets "no A2A
            // task" on reply instead of growing the map forever.
            const MAX_TASKS: usize = 4096;
            if tasks.len() >= MAX_TASKS
                && let Some(oldest) = tasks.keys().next().cloned()
            {
                tasks.remove(&oldest);
            }
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
            // The binding is spent: the task is complete, so the entry goes with it.
            Some(_) => {
                if let Ok(mut tasks) = self.tasks.lock() {
                    tasks.remove(chat_id);
                }
                SendResult::ok(task_id)
            }
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
        fail_sends: std::sync::atomic::AtomicBool,
        wake: Notify,
    }

    impl FakePlatform {
        fn new(events: Vec<MessageEvent>) -> Arc<Self> {
            Arc::new(Self {
                events: Mutex::new(events),
                sent: Mutex::new(Vec::new()),
                fail_sends: std::sync::atomic::AtomicBool::new(false),
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
            if self.fail_sends.load(std::sync::atomic::Ordering::SeqCst) {
                return SendResult::failed("boom", Some("transient"));
            }
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

    async fn wait_until(mut check: impl FnMut() -> bool) {
        let deadline = tokio::time::Instant::now() + std::time::Duration::from_secs(2);
        while !check() {
            assert!(tokio::time::Instant::now() < deadline, "timed out waiting");
            tokio::time::sleep(std::time::Duration::from_millis(10)).await;
        }
    }

    #[tokio::test]
    async fn a_successful_reply_is_recorded_delivered() {
        use crate::delivery::LedgerState;

        let mut inbound = event("chat-1", "hello");
        inbound.message_id = Some("m-1".to_string());
        let platform = FakePlatform::new(vec![inbound]);
        let ledger = Arc::new(crate::delivery::DeliveryLedger::in_memory().unwrap());
        let (stop_tx, stop_rx) = watch::channel(false);
        let task = tokio::spawn(serve_with_ledger(
            platform.clone(),
            echo(),
            stop_rx,
            Some(Arc::clone(&ledger)),
        ));
        wait_until(|| !platform.sent.lock().unwrap().is_empty()).await;
        stop_tx.send(true).unwrap();
        platform.wake.notify_waiters();
        let stats = task.await.unwrap();
        assert_eq!(stats.replies, 1);
        let entry = ledger.get("m-1").unwrap().expect("ledger row");
        assert_eq!(entry.state, LedgerState::Delivered);
        assert_eq!(entry.message_id.as_deref(), Some("m"));
        assert_eq!(entry.chat_id, "chat-1");
        assert_eq!(entry.body, "echo: hello");
    }

    #[tokio::test]
    async fn a_failed_reply_is_marked_failed_then_retried_by_the_sweep() {
        use crate::delivery::{DEFAULT_RETRY_BACKOFF_SECS, LedgerState, now_unix};

        let mut inbound = event("chat-1", "hello");
        inbound.message_id = Some("m-2".to_string());
        let platform = FakePlatform::new(vec![inbound]);
        platform
            .fail_sends
            .store(true, std::sync::atomic::Ordering::SeqCst);
        let ledger = Arc::new(crate::delivery::DeliveryLedger::in_memory().unwrap());
        let (stop_tx, stop_rx) = watch::channel(false);
        let task = tokio::spawn(serve_with_ledger(
            platform.clone(),
            echo(),
            stop_rx,
            Some(Arc::clone(&ledger)),
        ));
        // First attempt fails; the ledger owns the retry instead of dropping the reply.
        wait_until(|| !platform.sent.lock().unwrap().is_empty()).await;
        wait_until(|| {
            ledger
                .get("m-2")
                .unwrap()
                .is_some_and(|entry| entry.state == LedgerState::Failed)
        })
        .await;
        let failed = ledger.get("m-2").unwrap().unwrap();
        assert_eq!(failed.attempts, 1);
        assert_eq!(failed.last_error.as_deref(), Some("boom"));
        assert_eq!(failed.error_kind.as_deref(), Some("transient"));
        assert!(failed.flood_not_before > now_unix());

        // The platform recovers; once the backoff has elapsed the sweep resends and delivers.
        platform
            .fail_sends
            .store(false, std::sync::atomic::Ordering::SeqCst);
        let dyn_platform: Arc<dyn GatewayPlatform> = platform.clone();
        sweep_once(
            &dyn_platform,
            &ledger,
            now_unix() + DEFAULT_RETRY_BACKOFF_SECS + 1,
        )
        .await;
        let entry = ledger.get("m-2").unwrap().unwrap();
        assert_eq!(entry.state, LedgerState::Delivered);
        assert_eq!(entry.attempts, 2);
        assert_eq!(platform.sent.lock().unwrap().len(), 2);

        stop_tx.send(true).unwrap();
        platform.wake.notify_waiters();
        let _ = task.await.unwrap();
    }

    #[tokio::test]
    async fn the_sweep_reclaims_a_stale_attempt_and_sends_it() {
        use crate::delivery::{LedgerState, STALE_RECLAIM_AFTER_SECS};

        let ledger = crate::delivery::DeliveryLedger::in_memory().unwrap();
        let now = now_unix();
        ledger
            .enqueue(
                NewDelivery::new("stale-1", "fake", "chat-9", " stranded reply"),
                now,
            )
            .unwrap()
            .unwrap();
        ledger.claim("stale-1", now).unwrap().unwrap();
        assert_eq!(
            ledger.get("stale-1").unwrap().unwrap().state,
            LedgerState::Attempting
        );

        let platform = FakePlatform::new(vec![]);
        let dyn_platform: Arc<dyn GatewayPlatform> = platform.clone();
        // A claim this old belongs to a dead process; the sweep reclaims it and sends the body.
        sweep_once(&dyn_platform, &ledger, now + STALE_RECLAIM_AFTER_SECS + 1).await;
        let entry = ledger.get("stale-1").unwrap().unwrap();
        assert_eq!(entry.state, LedgerState::Delivered);
        assert_eq!(entry.attempts, 2);
        assert_eq!(
            platform.sent.lock().unwrap().as_slice(),
            &[("chat-9".to_string(), " stranded reply".to_string())]
        );
    }

    #[tokio::test]
    async fn a_redelivered_message_id_does_not_send_twice() {
        let mut first = event("chat-1", "hello");
        first.message_id = Some("dup-1".to_string());
        let mut second = event("chat-1", "hello again");
        second.message_id = Some("dup-1".to_string());
        let platform = FakePlatform::new(vec![first, second]);
        let ledger = Arc::new(crate::delivery::DeliveryLedger::in_memory().unwrap());
        let (stop_tx, stop_rx) = watch::channel(false);
        let task = tokio::spawn(serve_with_ledger(
            platform.clone(),
            echo(),
            stop_rx,
            Some(Arc::clone(&ledger)),
        ));
        wait_until(|| !platform.sent.lock().unwrap().is_empty()).await;
        // Give the loop a moment to (not) process the duplicate, then stop.
        tokio::time::sleep(std::time::Duration::from_millis(100)).await;
        stop_tx.send(true).unwrap();
        platform.wake.notify_waiters();
        let _ = task.await.unwrap();
        assert_eq!(platform.sent.lock().unwrap().len(), 1);
        assert_eq!(ledger.len().unwrap(), 1);
    }
}
