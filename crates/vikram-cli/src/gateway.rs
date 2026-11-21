//! `vikram gateway` — run configured chat platforms against the runtime.
//!
//! Each configured platform gets one host loop: read events, map the chat to a session, submit a
//! turn, then send the turn's text back. Configuration is environment-only for now, matching the
//! variable names in `docs/gateway-platforms.md`; a platform with no credentials is skipped with a
//! line explaining what is missing. Credentials resolve from the process environment first, then
//! `$VIKRAM_HOME/.env`, and `vikram gateway enable|disable` persists per-platform toggles in
//! `gateway.json` — a platform with no entry stays "auto" and follows the environment as before.
//!
//! The gateway is a headless surface, so it runs unattended: anything needing approval is refused
//! rather than waiting for a human who is not there.

use std::collections::HashMap;
use std::sync::{Arc, Mutex};

use crate::gateway_config::{self, env};
use vikram_contracts::{Block, ImageSource, Message, PermissionMode, Role, ToolCallId};
use vikram_gateway::access::{
    AuthzInput, EnvLookup, PairingVerdict, PlatformAuthSpec, authorize_source,
};
use vikram_gateway::delivery::{DeliveryLedger, now_unix};
use vikram_gateway::event::MessageEvent;
use vikram_gateway::host::{
    BuzzPlatform, ChannelPlatform, DingTalkStreamPlatform, DiscordPlatform, EmailPlatform,
    GatewayPlatform, IrcPlatform, MatrixPlatform, MattermostPlatform, NtfyPlatform, PhotonPlatform,
    SignalPlatform, SimplexPlatform, TelegramPlatform, WeComPlatform, WeixinPlatform,
    WhatsAppPlatform, YuanbaoPlatform, deliver_verb, serve_with_ledger,
};
use vikram_gateway::ingress::{
    WebhookHandler, WebhookHub, WebhookPlatform, event_route, serve as serve_webhooks,
};
use vikram_gateway::media_fetch::{MediaFetchLimits, fetch_images_with_skips};
use vikram_gateway::pairing::PairingStore;
use vikram_gateway::platform::{
    MediaKind, MediaSource as GatewayMediaSource, MessageHandler, OutboundMedia, OutboundVerb,
    PlatformAdapter,
};
use vikram_gateway::platforms::bluebubbles::BlueBubblesAdapter;
use vikram_gateway::platforms::buzz::{BuzzAdapter, BuzzConfig, BuzzKeypair};
use vikram_gateway::platforms::dingtalk_stream::Credential as DingTalkCredential;
use vikram_gateway::platforms::discord::{DiscordAdapter, DiscordGateway};
use vikram_gateway::platforms::email::EmailAdapter;
use vikram_gateway::platforms::feishu::FeishuAdapter;
use vikram_gateway::platforms::google_chat::GoogleChatAdapter;
use vikram_gateway::platforms::irc::IrcAdapter;
use vikram_gateway::platforms::line::LineAdapter;
use vikram_gateway::platforms::matrix::MatrixAdapter;
use vikram_gateway::platforms::mattermost::MattermostAdapter;
use vikram_gateway::platforms::ntfy::NtfyAdapter;
use vikram_gateway::platforms::photon::{PhotonAdapter, ReqwestSse};
use vikram_gateway::platforms::qqbot::QqBotAdapter;
use vikram_gateway::platforms::raft::{RaftAdapter, RaftConfig};
use vikram_gateway::platforms::signal::SignalAdapter;
use vikram_gateway::platforms::simplex::SimplexAdapter;
use vikram_gateway::platforms::slack::SlackAdapter;
use vikram_gateway::platforms::sms::SmsAdapter;
use vikram_gateway::platforms::teams::TeamsAdapter;
use vikram_gateway::platforms::telegram::TelegramAdapter;
use vikram_gateway::platforms::wecom::WeComAdapter;
use vikram_gateway::platforms::weixin::WeixinAdapter;
use vikram_gateway::platforms::whatsapp::WhatsAppAdapter;
use vikram_gateway::platforms::whatsapp_cloud::WhatsAppCloudAdapter;
use vikram_gateway::platforms::yuanbao::{YuanbaoAdapter, YuanbaoConfig};
use vikram_gateway::routing::{SessionKeyOptions, build_session_key};
use vikram_gateway::socket::{TcpLineSocket, TlsLineSocket, WsBinarySocket, WsSocket};
use vikram_gateway::transport::{ReqwestTransport, SharedTransport};
use vikram_runtime::config::{Config, vikram_home};
use vikram_runtime::types::RuntimeEvent;
use vikram_runtime::{Runtime, RuntimeHandle};

/// The text parts of an A2A `message/send`, joined the way the engine joins them.
fn a2a_text(request: &serde_json::Value) -> String {
    request
        .get("params")
        .and_then(|params| params.get("message"))
        .and_then(|message| message.get("parts"))
        .and_then(|parts| parts.as_array())
        .map(|parts| {
            parts
                .iter()
                .filter(|part| part.get("type").and_then(|t| t.as_str()) == Some("text"))
                .filter_map(|part| part.get("text").and_then(|t| t.as_str()))
                .collect::<Vec<_>>()
                .join("\n")
        })
        .unwrap_or_default()
        .trim()
        .to_string()
}

/// The process environment as [`EnvLookup`], through the same trim-and-drop-empty `env` helper
/// every credential read uses.
struct ProcessEnv;

impl EnvLookup for ProcessEnv {
    fn get(&self, key: &str) -> Option<String> {
        env(key)
    }
}

/// What the inbound path needs to run `access::authorize_source`: the environment view the
/// allowlists are read from, and the pairing store operator-approved codes are checked against.
struct IngressAuth {
    env: ProcessEnv,
    pairing: Option<Arc<PairingStore>>,
}

/// The per-platform slice of the central authorizer's configuration. The env names follow the
/// `<PLATFORM>_ALLOWED_USERS` / `_ALLOW_ALL_USERS` / `_GROUP_ALLOWED_CHATS` / `_ALLOW_BOTS`
/// convention every adapter documents, derived from the platform's own name; the global lists
/// (`GATEWAY_ALLOWED_USERS`, `GATEWAY_ALLOW_ALL_USERS`) are read inside `authorize_source` itself.
fn auth_spec(platform: &str) -> PlatformAuthSpec {
    let prefix = platform
        .chars()
        .map(|c| {
            if c.is_ascii_alphanumeric() {
                c.to_ascii_uppercase()
            } else {
                '_'
            }
        })
        .collect::<String>();
    let named = |suffix: &str| Some(format!("{prefix}_{suffix}"));
    PlatformAuthSpec {
        allowed_users_env: named("ALLOWED_USERS"),
        allow_all_env: named("ALLOW_ALL_USERS"),
        group_allowed_chats_env: named("GROUP_ALLOWED_CHATS"),
        allow_bots_env: named("ALLOW_BOTS"),
        // A2A's only ingress is the bearer-checked `/a2a` route, so by the time an event exists
        // the upstream credential was already verified (access.rs decision 1). Its events carry
        // no user id, which the central layer would otherwise deny outright.
        trusted_inbound: platform == "a2a",
        ..PlatformAuthSpec::default()
    }
}

/// Constant-time token comparison, the same shape every webhook verifier in this workspace uses.
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

pub async fn run(config: Config) -> Result<(), String> {
    let runtime = Runtime::start(config)
        .await
        .map_err(|err| format!("could not start runtime: {err}"))?;
    let handle = runtime.handle();

    let mut platforms: Vec<Arc<dyn GatewayPlatform>> = Vec::new();
    let mut skipped: Vec<String> = Vec::new();
    // Per-platform on/off from `vikram gateway enable|disable`; absence is "auto" and unchanged.
    let toggles = gateway_config::load_for_run();
    let mut disabled: Vec<&'static str> = Vec::new();

    if toggles.is_disabled("telegram") {
        disabled.push("telegram");
    } else if let Some(token) = env("TELEGRAM_BOT_TOKEN") {
        let transport = ReqwestTransport::new().map_err(|err| err.to_string())?;
        let adapter = TelegramAdapter::new(transport.into_shared(), token);
        // A mirror or a local Bot API server; unset means the public endpoint.
        let adapter = match env("TELEGRAM_API_BASE") {
            Some(base) => adapter.with_api_base(base),
            None => adapter,
        };
        platforms.push(Arc::new(TelegramPlatform::new(adapter)));
    } else {
        skipped.push("telegram (TELEGRAM_BOT_TOKEN)".into());
    }

    if toggles.is_disabled("discord") {
        disabled.push("discord");
    } else if let Some(token) = env("DISCORD_BOT_TOKEN") {
        let transport = ReqwestTransport::new().map_err(|err| err.to_string())?;
        let mut adapter = DiscordAdapter::new(transport.into_shared(), token.clone());
        if let Some(base) = env("DISCORD_API_BASE") {
            adapter = adapter.with_api_base(base);
        }
        let gateway_url = env("DISCORD_GATEWAY_URL")
            .unwrap_or_else(|| vikram_gateway::platforms::discord::DISCORD_GATEWAY_URL.to_string());
        let socket = WsSocket::connect(&gateway_url).map_err(|err| err.to_string())?;
        let gateway = DiscordGateway::new(socket.into_shared(), token);
        match gateway.identify().await {
            Ok(()) => platforms.push(Arc::new(DiscordPlatform::new(adapter, gateway))),
            Err(err) => eprintln!("[gateway] discord identify failed: {err}"),
        }
    } else {
        skipped.push("discord (DISCORD_BOT_TOKEN)".into());
    }

    match (
        toggles.is_disabled("signal"),
        env("SIGNAL_HTTP_URL"),
        env("SIGNAL_ACCOUNT"),
    ) {
        (true, _, _) => disabled.push("signal"),
        (false, Some(base), Some(account)) => {
            let transport = ReqwestTransport::new().map_err(|err| err.to_string())?;
            platforms.push(Arc::new(SignalPlatform::new(SignalAdapter::new(
                transport.into_shared(),
                base,
                account,
            ))));
        }
        _ => skipped.push("signal (SIGNAL_HTTP_URL, SIGNAL_ACCOUNT)".into()),
    }

    match (toggles.is_disabled("ntfy"), env("NTFY_TOPIC")) {
        (true, _) => disabled.push("ntfy"),
        (false, Some(topic)) => {
            let transport = ReqwestTransport::new().map_err(|err| err.to_string())?;
            let server = env("NTFY_SERVER_URL").unwrap_or_else(|| "https://ntfy.sh".into());
            platforms.push(Arc::new(NtfyPlatform::new(NtfyAdapter::new(
                transport.into_shared(),
                server,
                topic,
                env("NTFY_TOKEN"),
            ))));
        }
        _ => skipped.push("ntfy (NTFY_TOPIC)".into()),
    }

    match (
        toggles.is_disabled("weixin"),
        env("WEIXIN_ACCOUNT_ID"),
        env("WEIXIN_TOKEN"),
    ) {
        (true, _, _) => disabled.push("weixin"),
        (false, Some(account), Some(token)) => {
            let transport = ReqwestTransport::new().map_err(|err| err.to_string())?;
            let base =
                env("WEIXIN_BASE_URL").unwrap_or_else(|| "https://ilinkai.weixin.qq.com".into());
            platforms.push(Arc::new(WeixinPlatform::new(WeixinAdapter::new(
                transport.into_shared(),
                base,
                account,
                token,
            ))));
        }
        _ => skipped.push("weixin (WEIXIN_ACCOUNT_ID, WEIXIN_TOKEN)".into()),
    }

    if toggles.is_disabled("simplex") {
        disabled.push("simplex");
    } else if let Some(url) = env("SIMPLEX_WS_URL") {
        let socket = WsSocket::connect(&url).map_err(|err| err.to_string())?;
        platforms.push(Arc::new(SimplexPlatform::new(SimplexAdapter::new(
            socket.into_shared(),
        ))));
    } else {
        skipped.push("simplex (SIMPLEX_WS_URL)".into());
    }

    match (
        toggles.is_disabled("irc"),
        env("IRC_SERVER"),
        env("IRC_NICKNAME"),
    ) {
        (true, _, _) => disabled.push("irc"),
        (false, Some(server), Some(nickname)) => {
            let port = env("IRC_PORT")
                .and_then(|value| value.parse::<u16>().ok())
                .unwrap_or(6667);
            let socket = TcpLineSocket::connect(&server, port)
                .await
                .map_err(|err| err.to_string())?;
            let channel = env("IRC_CHANNEL").unwrap_or_default();
            platforms.push(Arc::new(IrcPlatform::new(IrcAdapter::new(
                socket.into_shared(),
                nickname,
                channel,
            ))));
        }
        _ => skipped.push("irc (IRC_SERVER, IRC_NICKNAME)".into()),
    }

    match (
        toggles.is_disabled("matrix"),
        env("MATRIX_HOMESERVER"),
        env("MATRIX_ACCESS_TOKEN"),
    ) {
        (true, _, _) => disabled.push("matrix"),
        (false, Some(homeserver), Some(token)) => {
            let transport = ReqwestTransport::new().map_err(|err| err.to_string())?;
            let adapter = MatrixAdapter::new(transport.into_shared(), homeserver, token);
            platforms.push(Arc::new(MatrixPlatform::new(adapter)));
        }
        _ => skipped.push("matrix (MATRIX_HOMESERVER, MATRIX_ACCESS_TOKEN)".into()),
    }

    match (
        toggles.is_disabled("mattermost"),
        env("MATTERMOST_URL"),
        env("MATTERMOST_TOKEN"),
        env("MATTERMOST_CHANNEL_ID"),
    ) {
        (true, _, _, _) => disabled.push("mattermost"),
        (false, Some(base), Some(token), Some(channel)) => {
            let transport = ReqwestTransport::new().map_err(|err| err.to_string())?;
            let adapter = MattermostAdapter::new(transport.into_shared(), base, token);
            platforms.push(Arc::new(MattermostPlatform::new(adapter, channel)));
        }
        _ => skipped
            .push("mattermost (MATTERMOST_URL, MATTERMOST_TOKEN, MATTERMOST_CHANNEL_ID)".into()),
    }

    if toggles.is_disabled("whatsapp") {
        disabled.push("whatsapp");
    } else if let Some(base) = env("WHATSAPP_BRIDGE_URL") {
        let transport = ReqwestTransport::new().map_err(|err| err.to_string())?;
        platforms.push(Arc::new(WhatsAppPlatform::new(WhatsAppAdapter::new(
            transport.into_shared(),
            base,
        ))));
    } else {
        skipped.push("whatsapp (WHATSAPP_BRIDGE_URL)".into());
    }

    match (
        toggles.is_disabled("wecom"),
        env("WECOM_BOT_ID"),
        env("WECOM_SECRET"),
    ) {
        (true, _, _) => disabled.push("wecom"),
        (false, Some(bot_id), Some(secret)) => {
            let url = env("WECOM_WEBSOCKET_URL")
                .unwrap_or_else(|| vikram_gateway::platforms::wecom::WECOM_WS_URL.to_string());
            let socket = WsSocket::connect(&url).map_err(|err| err.to_string())?;
            let device = env("WECOM_DEVICE_ID").unwrap_or_else(|| "vikram".to_string());
            let mut adapter = WeComAdapter::new(socket.into_shared(), bot_id, secret, device);
            if adapter.connect(false).await {
                platforms.push(Arc::new(WeComPlatform::new(adapter)));
            } else {
                eprintln!("[gateway] wecom subscription failed; platform skipped");
            }
        }
        _ => skipped.push("wecom (WECOM_BOT_ID, WECOM_SECRET)".into()),
    }

    match (
        toggles.is_disabled("email"),
        env("EMAIL_FROM"),
        env("EMAIL_SMTP_HOST"),
        env("EMAIL_IMAP_HOST"),
    ) {
        (true, _, _, _) => disabled.push("email"),
        (false, Some(from), Some(smtp_host), Some(imap_host)) => {
            let tls = env("EMAIL_TLS").map(|v| v != "false").unwrap_or(true);
            let smtp_port = env("EMAIL_SMTP_PORT")
                .and_then(|v| v.parse::<u16>().ok())
                .unwrap_or(if tls { 465 } else { 25 });
            let imap_port = env("EMAIL_IMAP_PORT")
                .and_then(|v| v.parse::<u16>().ok())
                .unwrap_or(if tls { 993 } else { 143 });

            let smtp: vikram_gateway::socket::SharedLineSocket = if tls {
                Arc::new(
                    TlsLineSocket::connect(&smtp_host, smtp_port)
                        .await
                        .map_err(|err| format!("email smtp: {err}"))?,
                )
            } else {
                Arc::new(
                    TcpLineSocket::connect(&smtp_host, smtp_port)
                        .await
                        .map_err(|err| format!("email smtp: {err}"))?,
                )
            };
            let imap: vikram_gateway::socket::SharedLineSocket = if tls {
                Arc::new(
                    TlsLineSocket::connect(&imap_host, imap_port)
                        .await
                        .map_err(|err| format!("email imap: {err}"))?,
                )
            } else {
                Arc::new(
                    TcpLineSocket::connect(&imap_host, imap_port)
                        .await
                        .map_err(|err| format!("email imap: {err}"))?,
                )
            };
            let mut adapter = EmailAdapter::new(
                smtp,
                Some(imap),
                from,
                env("EMAIL_MAILBOX").unwrap_or_else(|| "INBOX".into()),
            );
            if let (Some(user), Some(password)) = (env("EMAIL_USERNAME"), env("EMAIL_PASSWORD")) {
                adapter = adapter.with_credentials(user, password);
            }
            if adapter.connect(false).await {
                platforms.push(Arc::new(EmailPlatform::new(adapter)));
            } else {
                eprintln!("[gateway] email SMTP handshake failed; platform skipped");
            }
        }
        _ => skipped.push(
            "email (EMAIL_FROM, EMAIL_SMTP_HOST, EMAIL_IMAP_HOST; optional EMAIL_TLS, EMAIL_USERNAME, EMAIL_PASSWORD, EMAIL_MAILBOX)"
                .into(),
        ),
    }

    match (
        toggles.is_disabled("yuanbao"),
        env("YUANBAO_APP_ID"),
        env("YUANBAO_APP_SECRET"),
    ) {
        (true, _, _) => disabled.push("yuanbao"),
        (false, Some(app_id), Some(app_secret)) => {
            let transport = ReqwestTransport::new().map_err(|err| err.to_string())?;
            let base = YuanbaoConfig::default();
            let config = YuanbaoConfig {
                app_id,
                app_secret,
                bot_id: env("YUANBAO_BOT_ID").unwrap_or_default(),
                ws_url: env("YUANBAO_WS_URL").unwrap_or(base.ws_url),
                api_domain: env("YUANBAO_API_DOMAIN").unwrap_or(base.api_domain),
                ..base
            };
            let socket = Arc::new(
                WsBinarySocket::connect(&config.ws_url)
                    .map_err(|err| format!("yuanbao socket: {err}"))?,
            );
            let mut adapter = YuanbaoAdapter::new(config, socket, transport.into_shared());
            if adapter.connect(false).await {
                platforms.push(Arc::new(YuanbaoPlatform::new(adapter)));
            } else {
                eprintln!("[gateway] yuanbao auth bind failed; platform skipped");
            }
        }
        _ => skipped.push(
            "yuanbao (YUANBAO_APP_ID, YUANBAO_APP_SECRET; optional YUANBAO_BOT_ID, YUANBAO_WS_URL)"
                .into(),
        ),
    }

    match (
        toggles.is_disabled("buzz"),
        env("BUZZ_RELAY_URL"),
        env("BUZZ_PRIVATE_KEY"),
    ) {
        (true, _, _) => disabled.push("buzz"),
        (false, Some(_), Some(_)) => match BuzzKeypair::from_env() {
            Ok(keys) => match BuzzAdapter::dial(keys, BuzzConfig::from_env()) {
                Ok(mut adapter) => {
                    if adapter.connect(false).await {
                        platforms.push(Arc::new(BuzzPlatform::new(adapter)));
                    } else {
                        eprintln!("[gateway] buzz relay rejected the session; platform skipped");
                    }
                }
                Err(err) => eprintln!("[gateway] buzz socket failed: {err}; platform skipped"),
            },
            Err(err) => eprintln!("[gateway] buzz key invalid: {err}; platform skipped"),
        },
        _ => skipped.push("buzz (BUZZ_RELAY_URL, BUZZ_PRIVATE_KEY)".into()),
    }

    match (
        toggles.is_disabled("dingtalk"),
        env("DINGTALK_CLIENT_ID"),
        env("DINGTALK_CLIENT_SECRET"),
    ) {
        (true, _, _) => disabled.push("dingtalk"),
        (false, Some(client_id), Some(client_secret)) => {
            let transport = ReqwestTransport::new().map_err(|err| err.to_string())?;
            let stream = vikram_gateway::platforms::dingtalk_stream::DingTalkStream::new(
                DingTalkCredential::new(client_id, client_secret),
                transport.into_shared(),
            );
            match stream.connect().await {
                Ok(_) => platforms.push(Arc::new(DingTalkStreamPlatform::new(stream))),
                Err(err) => {
                    eprintln!("[gateway] dingtalk stream connect failed: {err}; platform skipped")
                }
            }
        }
        _ => skipped.push("dingtalk stream (DINGTALK_CLIENT_ID, DINGTALK_CLIENT_SECRET)".into()),
    }

    match (
        toggles.is_disabled("photon"),
        env("PHOTON_SIDECAR_URL"),
        env("PHOTON_SIDECAR_TOKEN"),
    ) {
        (true, _, _) => disabled.push("photon"),
        (false, Some(url), Some(token)) => {
            let transport = ReqwestTransport::new().map_err(|err| err.to_string())?;
            let adapter = PhotonAdapter::new(transport.into_shared(), url.clone(), token.clone());
            let stream_url = format!("{}/inbound", url.trim_end_matches('/'));
            let source = Arc::new(ReqwestSse::connect(stream_url, token)?);
            platforms.push(Arc::new(PhotonPlatform::new(adapter, source)));
        }
        _ => skipped.push("photon (PHOTON_SIDECAR_URL, PHOTON_SIDECAR_TOKEN)".into()),
    }

    // Webhook-shaped platforms need the ingress listener; build their routes only when a bind
    // address is configured.
    let mut webhook_hub = WebhookHub::new();
    let mut webhook_routes = 0usize;

    match (
        toggles.is_disabled("bluebubbles"),
        env("BLUEBUBBLES_SERVER_URL"),
        env("BLUEBUBBLES_PASSWORD"),
    ) {
        (true, _, _) => disabled.push("bluebubbles"),
        (false, Some(server), Some(password)) => {
            let transport = ReqwestTransport::new().map_err(|err| err.to_string())?;
            let adapter = Arc::new(BlueBubblesAdapter::new(
                transport.into_shared(),
                server,
                password.clone(),
            ));
            let platform = WebhookPlatform::new("bluebubbles", adapter.clone());
            let sender = platform.sender();
            let route_adapter = Arc::clone(&adapter);
            webhook_hub = webhook_hub.route(
                "/bluebubbles",
                event_route(move |request| {
                    let adapter = Arc::clone(&route_adapter);
                    let sender = sender.clone();
                    let password = password.clone();
                    Box::pin(async move {
                        // BlueBubbles documents the server password as a query parameter (the
                        // reference also accepts `x-password`). Refuse anything else outright.
                        let query = parse_query(&request.path);
                        let presented = query_value(&query, "password")
                            .map(str::to_string)
                            .or_else(|| request.header("x-password").map(str::to_string));
                        if presented.as_deref() != Some(password.as_str()) {
                            return (403, "bad password".to_string());
                        }
                        let Ok(text) = request.body_text() else {
                            return (400, "body is not utf-8".to_string());
                        };
                        match adapter.parse_webhook(text) {
                            Ok(events) => {
                                for event in events {
                                    let _ = sender.send(event).await;
                                }
                                (200, "ok".to_string())
                            }
                            Err(err) => (400, err),
                        }
                    })
                }),
            );
            platforms.push(platform);
            webhook_routes += 1;
        }
        _ => skipped.push("bluebubbles (BLUEBUBBLES_SERVER_URL, BLUEBUBBLES_PASSWORD)".into()),
    }

    match (
        toggles.is_disabled("whatsapp_cloud"),
        env("WHATSAPP_CLOUD_ACCESS_TOKEN"),
        env("WHATSAPP_CLOUD_PHONE_NUMBER_ID"),
        env("WHATSAPP_CLOUD_APP_SECRET"),
        env("WHATSAPP_CLOUD_VERIFY_TOKEN"),
    ) {
        (true, _, _, _, _) => disabled.push("whatsapp_cloud"),
        (
            false,
            Some(token),
            Some(phone_id),
            Some(app_secret),
            Some(verify_token),
        ) => {
            let transport = ReqwestTransport::new().map_err(|err| err.to_string())?;
            let adapter = Arc::new(
                WhatsAppCloudAdapter::new(transport.into_shared(), token, phone_id)
                    .with_webhook_secrets(app_secret, verify_token),
            );
            let platform = WebhookPlatform::new("whatsapp_cloud", adapter.clone());
            let sender = platform.sender();
            let route_adapter = Arc::clone(&adapter);
            webhook_hub = webhook_hub.route(
                "/whatsapp_cloud",
                event_route(move |request| {
                    let adapter = Arc::clone(&route_adapter);
                    let sender = sender.clone();
                    Box::pin(async move {
                        let query = parse_query(&request.path);
                        match request.method.as_str() {
                            "GET" => {
                                let mode = query_value(&query, "hub.mode").unwrap_or_default();
                                let token = query_value(&query, "hub.verify_token").unwrap_or_default();
                                let challenge =
                                    query_value(&query, "hub.challenge").unwrap_or_default();
                                match adapter.verify_webhook(mode, token, challenge) {
                                    Ok(challenge) => (200, challenge),
                                    Err(err) => (403, err),
                                }
                            }
                            "POST" => {
                                let signature = request
                                    .header("X-Hub-Signature-256")
                                    .unwrap_or_default()
                                    .to_string();
                                if !adapter.verify_signature(&request.body, &signature) {
                                    return (403, "bad signature".to_string());
                                }
                                let Ok(text) = request.body_text() else {
                                    return (400, "body is not utf-8".to_string());
                                };
                                match adapter.parse_webhook(text) {
                                    Ok(events) => {
                                        for event in events {
                                            let _ = sender.send(event).await;
                                        }
                                        (200, "ok".to_string())
                                    }
                                    Err(err) => (400, err),
                                }
                            }
                            _ => (405, "method not allowed".to_string()),
                        }
                    })
                }),
            );
            platforms.push(platform);
            webhook_routes += 1;
        }
        _ => skipped.push(
            "whatsapp_cloud webhook (WHATSAPP_CLOUD_ACCESS_TOKEN, WHATSAPP_CLOUD_PHONE_NUMBER_ID, WHATSAPP_CLOUD_APP_SECRET, WHATSAPP_CLOUD_VERIFY_TOKEN)"
                .into(),
        ),
    }

    match (
        toggles.is_disabled("sms"),
        env("TWILIO_ACCOUNT_SID"),
        env("TWILIO_AUTH_TOKEN"),
        env("TWILIO_FROM_NUMBER"),
        env("TWILIO_WEBHOOK_URL"),
    ) {
        (true, _, _, _, _) => disabled.push("sms"),
        (false, Some(account_sid), Some(auth_token), Some(from), Some(webhook_url)) => {
            let transport = ReqwestTransport::new().map_err(|err| err.to_string())?;
            let adapter = Arc::new(SmsAdapter::new(
                transport.into_shared(),
                account_sid,
                auth_token,
                from,
            ));
            let platform = WebhookPlatform::new("sms", adapter.clone());
            let sender = platform.sender();
            let route_adapter = Arc::clone(&adapter);
            webhook_hub = webhook_hub.route(
                "/sms",
                event_route(move |request| {
                    let adapter = Arc::clone(&route_adapter);
                    let sender = sender.clone();
                    let webhook_url = webhook_url.clone();
                    Box::pin(async move {
                        let Ok(text) = request.body_text() else {
                            return (400, "body is not utf-8".to_string());
                        };
                        let params = parse_form(text);
                        let borrowed: Vec<(&str, String)> = params
                            .iter()
                            .map(|(key, value)| (key.as_str(), value.clone()))
                            .collect();
                        let signature = request
                            .header("X-Twilio-Signature")
                            .unwrap_or_default()
                            .to_string();
                        if !adapter.verify_webhook(&webhook_url, &borrowed, &signature) {
                            return (403, "bad signature".to_string());
                        }
                        match adapter.parse_webhook(&borrowed) {
                            Ok(Some(event)) => {
                                let _ = sender.send(event).await;
                                (200, "ok".to_string())
                            }
                            Ok(None) => (200, "ignored".to_string()),
                            Err(err) => (400, err),
                        }
                    })
                }),
            );
            platforms.push(platform);
            webhook_routes += 1;
        }
        _ => skipped.push(
            "sms webhook (TWILIO_ACCOUNT_SID, TWILIO_AUTH_TOKEN, TWILIO_FROM_NUMBER, TWILIO_WEBHOOK_URL)"
                .into(),
        ),
    }

    let webhook_bind = env("GATEWAY_WEBHOOK_BIND");

    if webhook_bind.is_some() {
        if toggles.is_disabled("feishu") {
            disabled.push("feishu");
        } else if let (Some(app_id), Some(app_secret)) =
            (env("FEISHU_APP_ID"), env("FEISHU_APP_SECRET"))
        {
            let transport = ReqwestTransport::new().map_err(|err| err.to_string())?;
            let mut adapter = FeishuAdapter::new(transport.into_shared(), app_id, app_secret);
            if let Some(token) = env("FEISHU_VERIFICATION_TOKEN") {
                adapter = adapter.with_verification_token(token);
            }
            if let Some(base) = env("FEISHU_API_BASE") {
                adapter = adapter.with_api_base(base);
            }
            let adapter = Arc::new(adapter);
            let platform = WebhookPlatform::new("feishu", adapter.clone());
            let sender = platform.sender();
            let route_adapter = Arc::clone(&adapter);
            webhook_hub = webhook_hub.route(
                "/feishu",
                event_route(move |request| {
                    let adapter = Arc::clone(&route_adapter);
                    let sender = sender.clone();
                    Box::pin(async move {
                        let Ok(text) = request.body_text() else {
                            return (400, "body is not utf-8".to_string());
                        };
                        let Ok(value) = serde_json::from_str::<serde_json::Value>(text) else {
                            return (400, "body is not json".to_string());
                        };
                        // Feishu's subscription handshake is a challenge, not an event.
                        if value.get("type").and_then(|t| t.as_str()) == Some("url_verification") {
                            let challenge = value
                                .get("challenge")
                                .and_then(|c| c.as_str())
                                .unwrap_or_default()
                                .to_string();
                            return (200, challenge);
                        }
                        if !adapter.verify_webhook(&value) {
                            return (403, "verification token mismatch".to_string());
                        }
                        match adapter.parse_webhook(text) {
                            Ok(events) => {
                                for event in events {
                                    let _ = sender.send(event).await;
                                }
                                (200, "ok".to_string())
                            }
                            Err(err) => (400, err),
                        }
                    })
                }),
            );
            platforms.push(platform);
            webhook_routes += 1;
        } else {
            skipped.push("feishu webhook (FEISHU_APP_ID, FEISHU_APP_SECRET)".into());
        }

        match (
            toggles.is_disabled("line"),
            env("LINE_CHANNEL_ACCESS_TOKEN"),
            env("LINE_CHANNEL_SECRET"),
        ) {
            (true, _, _) => disabled.push("line"),
            (false, Some(token), Some(secret)) => {
                let transport = ReqwestTransport::new().map_err(|err| err.to_string())?;
                let adapter = Arc::new(LineAdapter::new(transport.into_shared(), token, secret));
                let platform = WebhookPlatform::new("line", adapter.clone());
                let sender = platform.sender();
                let route_adapter = Arc::clone(&adapter);
                webhook_hub = webhook_hub.route(
                    "/line",
                    event_route(move |request| {
                        let adapter = Arc::clone(&route_adapter);
                        let sender = sender.clone();
                        Box::pin(async move {
                            let signature = request
                                .header("X-Line-Signature")
                                .unwrap_or_default()
                                .to_string();
                            if !adapter.verify_webhook(&request.body, &signature) {
                                return (403, "bad signature".to_string());
                            }
                            let Ok(text) = request.body_text() else {
                                return (400, "body is not utf-8".to_string());
                            };
                            match adapter.parse_webhook(text) {
                                Ok(events) => {
                                    for event in events {
                                        let _ = sender.send(event).await;
                                    }
                                    (200, "ok".to_string())
                                }
                                Err(err) => (400, err),
                            }
                        })
                    }),
                );
                platforms.push(platform);
                webhook_routes += 1;
            }
            _ => {
                skipped.push("line webhook (LINE_CHANNEL_ACCESS_TOKEN, LINE_CHANNEL_SECRET)".into())
            }
        }

        match (
            toggles.is_disabled("slack"),
            env("SLACK_BOT_TOKEN"),
            env("SLACK_SIGNING_SECRET"),
        ) {
            (true, _, _) => disabled.push("slack"),
            (false, Some(bot_token), Some(secret)) => {
                let transport = ReqwestTransport::new().map_err(|err| err.to_string())?;
                let mut adapter = SlackAdapter::new(transport.into_shared(), bot_token)
                    .with_signing_secret(secret);
                if let Some(base) = env("SLACK_API_BASE") {
                    adapter = adapter.with_api_base(base);
                }
                let adapter = Arc::new(adapter);
                let platform = WebhookPlatform::new("slack", adapter.clone());
                let sender = platform.sender();
                let route_adapter = Arc::clone(&adapter);
                webhook_hub = webhook_hub.route(
                    "/slack",
                    event_route(move |request| {
                        let adapter = Arc::clone(&route_adapter);
                        let sender = sender.clone();
                        Box::pin(async move {
                            let Ok(text) = request.body_text() else {
                                return (400, "body is not utf-8".to_string());
                            };
                            // Slack's endpoint handshake carries no signature.
                            if let Some(challenge) = adapter.webhook_challenge(text) {
                                return (200, challenge);
                            }
                            let timestamp = request
                                .header("X-Slack-Request-Timestamp")
                                .unwrap_or_default()
                                .to_string();
                            let signature = request
                                .header("X-Slack-Signature")
                                .unwrap_or_default()
                                .to_string();
                            if !adapter.verify_webhook(&request.body, &timestamp, &signature) {
                                return (403, "bad signature".to_string());
                            }
                            match adapter.parse_webhook(text) {
                                Ok(events) => {
                                    for event in events {
                                        let _ = sender.send(event).await;
                                    }
                                    (200, "ok".to_string())
                                }
                                Err(err) => (400, err),
                            }
                        })
                    }),
                );
                platforms.push(platform);
                webhook_routes += 1;
            }
            _ => skipped.push("slack webhook (SLACK_BOT_TOKEN, SLACK_SIGNING_SECRET)".into()),
        }

        // QQ Bot signs webhook POSTs with Ed25519 over `timestamp ++ body`.
        match (
            toggles.is_disabled("qqbot"),
            env("QQ_BOT_APP_ID"),
            env("QQ_BOT_SECRET"),
        ) {
            (true, _, _) => disabled.push("qqbot"),
            (false, Some(app_id), Some(secret)) => {
                let transport = ReqwestTransport::new().map_err(|err| err.to_string())?;
                let adapter = Arc::new(QqBotAdapter::new(transport.into_shared(), app_id, secret));
                if !adapter.has_webhook_secret() {
                    skipped.push(
                        "qqbot webhook (QQ_BOT_SECRET must be the 32-byte signing seed)".into(),
                    );
                } else {
                    let platform = WebhookPlatform::new("qqbot", adapter.clone());
                    let sender = platform.sender();
                    let route_adapter = Arc::clone(&adapter);
                    webhook_hub = webhook_hub.route(
                        "/qqbot",
                        event_route(move |request| {
                            let adapter = Arc::clone(&route_adapter);
                            let sender = sender.clone();
                            Box::pin(async move {
                                let timestamp = request
                                    .header("X-Signature-Timestamp")
                                    .unwrap_or_default()
                                    .to_string();
                                let signature = request
                                    .header("X-Signature-Ed25519")
                                    .unwrap_or_default()
                                    .to_string();
                                let Ok(text) = request.body_text() else {
                                    return (400, "body is not utf-8".to_string());
                                };
                                if !adapter.verify_webhook(&timestamp, &signature, text.as_bytes())
                                {
                                    return (403, "bad signature".to_string());
                                }
                                match adapter.parse_webhook(text) {
                                    Ok(events) => {
                                        for event in events {
                                            let _ = sender.send(event).await;
                                        }
                                        (200, "ok".to_string())
                                    }
                                    Err(err) => (400, err),
                                }
                            })
                        }),
                    );
                    platforms.push(platform);
                    webhook_routes += 1;
                }
            }
            _ => skipped.push("qqbot webhook (QQ_BOT_APP_ID, QQ_BOT_SECRET)".into()),
        }

        // Raft is a wake-only local bridge: it answers HTTP and hands the host a content-free hint.
        // Opt in with a bridge token — the routes are served on the same bind as every other
        // webhook, so enabling it must be explicit.
        match (toggles.is_disabled("raft"), env("RAFT_BRIDGE_TOKEN")) {
            (true, _) => disabled.push("raft"),
            (false, Some(token)) => {
                let mut raft = RaftAdapter::new(RaftConfig {
                    host: env("RAFT_HOST").unwrap_or_else(|| "127.0.0.1".into()),
                    port: env("RAFT_PORT")
                        .and_then(|value| value.parse::<u16>().ok())
                        .unwrap_or(0),
                    path: env("RAFT_PATH").unwrap_or_else(|| "/wake".into()),
                    bridge_token: Some(token),
                    runtime_session: env("RAFT_RUNTIME_SESSION")
                        .unwrap_or_else(|| "default".into()),
                    max_body_bytes: 16_384,
                    dedup_window_ms: 300_000,
                    dedup_cap: 512,
                    activity_queue_cap: 500,
                });
                let (tx, rx) = tokio::sync::mpsc::channel(64);
                let handler: MessageHandler = Arc::new(move |event| {
                    let tx = tx.clone();
                    Box::pin(async move {
                        let _ = tx.send(event).await;
                        None
                    })
                });
                raft.set_message_handler(handler);
                let raft = Arc::new(raft);
                for path in raft.routes() {
                    webhook_hub = webhook_hub.route(path, raft.clone());
                }
                platforms.push(Arc::new(ChannelPlatform::new(
                    "raft",
                    rx,
                    "raft is wake-only: reply through the raft CLI",
                )));
                webhook_routes += 4;
            }
            _ => skipped.push(
                "raft (RAFT_BRIDGE_TOKEN; optional RAFT_RUNTIME_SESSION, RAFT_PATH, RAFT_HOST, RAFT_PORT)"
                    .into(),
            ),
        }

        // Google Chat signs its webhook POSTs with a Google JWT.
        match (
            toggles.is_disabled("google_chat"),
            env("GOOGLE_CHAT_WEBHOOK_URL"),
            env("GOOGLE_CHAT_PROJECT_NUMBER"),
        ) {
            (true, _, _) => disabled.push("google_chat"),
            (false, Some(webhook_url), Some(project)) => {
                let transport = ReqwestTransport::new().map_err(|err| err.to_string())?;
                let adapter = Arc::new(
                    GoogleChatAdapter::new(transport.into_shared(), webhook_url)
                        .with_project_number(project),
                );
                let platform = WebhookPlatform::new("google_chat", adapter.clone());
                let sender = platform.sender();
                let route_adapter = Arc::clone(&adapter);
                webhook_hub = webhook_hub.route(
                    "/google_chat",
                    event_route(move |request| {
                        let adapter = Arc::clone(&route_adapter);
                        let sender = sender.clone();
                        Box::pin(async move {
                            let authorization = request
                                .header("Authorization")
                                .unwrap_or_default()
                                .to_string();
                            if !adapter.verify_request(&authorization).await {
                                return (403, "bad JWT".to_string());
                            }
                            let Ok(text) = request.body_text() else {
                                return (400, "body is not utf-8".to_string());
                            };
                            match adapter.parse_webhook(text) {
                                Ok(events) => {
                                    for event in events {
                                        let _ = sender.send(event).await;
                                    }
                                    (200, "ok".to_string())
                                }
                                Err(err) => (400, err),
                            }
                        })
                    }),
                );
                platforms.push(platform);
                webhook_routes += 1;
            }
            _ => skipped.push(
                "google_chat webhook (GOOGLE_CHAT_WEBHOOK_URL, GOOGLE_CHAT_PROJECT_NUMBER)".into(),
            ),
        }

        // Teams posts Bot Framework activities signed with a Microsoft JWT.
        match (
            toggles.is_disabled("teams"),
            env("TEAMS_TENANT_ID"),
            env("TEAMS_CLIENT_ID"),
            env("TEAMS_CLIENT_SECRET"),
        ) {
            (true, _, _, _) => disabled.push("teams"),
            (false, Some(tenant), Some(client_id), Some(client_secret)) => {
                let transport = ReqwestTransport::new().map_err(|err| err.to_string())?;
                let adapter = Arc::new(TeamsAdapter::new(
                    transport.into_shared(),
                    tenant,
                    client_id,
                    client_secret,
                ));
                let platform = WebhookPlatform::new("teams", adapter.clone());
                let sender = platform.sender();
                let route_adapter = Arc::clone(&adapter);
                webhook_hub = webhook_hub.route(
                    "/teams",
                    event_route(move |request| {
                        let adapter = Arc::clone(&route_adapter);
                        let sender = sender.clone();
                        Box::pin(async move {
                            let authorization = request
                                .header("Authorization")
                                .unwrap_or_default()
                                .to_string();
                            if !adapter.verify_request(&authorization).await {
                                return (403, "bad JWT".to_string());
                            }
                            let Ok(text) = request.body_text() else {
                                return (400, "body is not utf-8".to_string());
                            };
                            match adapter.parse_webhook(text) {
                                Ok(events) => {
                                    for event in events {
                                        let _ = sender.send(event).await;
                                    }
                                    (200, "ok".to_string())
                                }
                                Err(err) => (400, err),
                            }
                        })
                    }),
                );
                platforms.push(platform);
                webhook_routes += 1;
            }
            _ => skipped.push(
                "teams webhook (TEAMS_TENANT_ID, TEAMS_CLIENT_ID, TEAMS_CLIENT_SECRET)".into(),
            ),
        }

        // A2A is agent-facing: an unauthenticated `/a2a` would let anyone who can reach the bind
        // submit turns to the runtime, so the route requires `A2A_TOKEN` as a bearer credential —
        // the same shared-secret shape the other webhook routes verify. Without it configured the
        // route refuses rather than running open.
        if toggles.is_disabled("a2a") {
            disabled.push("a2a");
        } else {
            let a2a_token = env("A2A_TOKEN");
            let engine = Arc::new(vikram_gateway::platforms::a2a::A2aEngine::new());
            let a2a = vikram_gateway::host::A2aPlatform::new(Arc::clone(&engine));
            let sender = a2a.sender();
            let route_engine = Arc::clone(&engine);
            let route_platform = Arc::clone(&a2a);
            webhook_hub = webhook_hub.route(
                "/a2a",
                event_route(move |request| {
                    let engine = Arc::clone(&route_engine);
                    let platform = Arc::clone(&route_platform);
                    let sender = sender.clone();
                    let a2a_token = a2a_token.clone();
                    Box::pin(async move {
                        if request.method != "POST" {
                            return (405, "method not allowed".to_string());
                        }
                        match a2a_token.as_deref() {
                            Some(expected) => {
                                let authorization =
                                    request.header("Authorization").unwrap_or_default();
                                let presented =
                                    authorization.strip_prefix("Bearer ").unwrap_or_default();
                                if !constant_time_eq(presented.as_bytes(), expected.as_bytes()) {
                                    return (401, "bad token".to_string());
                                }
                            }
                            None => return (403, "a2a is not configured".to_string()),
                        }
                        let Ok(text) = request.body_text() else {
                            return (400, "body is not utf-8".to_string());
                        };
                        let Ok(value) = serde_json::from_str::<serde_json::Value>(text) else {
                            return (400, "body is not json".to_string());
                        };
                        let response = engine.handle_jsonrpc(&value);
                        if value.get("method").and_then(|m| m.as_str()) == Some("message/send")
                            && let Some(task) = response.get("result")
                        {
                            let task_id = task.get("id").and_then(|v| v.as_str());
                            let context_id = task.get("contextId").and_then(|v| v.as_str());
                            if let (Some(task_id), Some(context_id)) = (task_id, context_id) {
                                platform.remember(context_id, task_id);
                                let prompt = a2a_text(&value);
                                if !prompt.is_empty() {
                                    let platform_name = vikram_gateway::event::Platform::new("a2a")
                                        .expect("valid platform");
                                    let mut source = vikram_gateway::event::SessionSource::new(
                                        platform_name,
                                        context_id,
                                    );
                                    source.chat_type = "dm".to_string();
                                    let mut event =
                                        vikram_gateway::event::MessageEvent::new(prompt, source);
                                    event
                                        .metadata
                                        .insert("a2a_task".to_string(), serde_json::json!(task_id));
                                    let _ = sender.send(event).await;
                                }
                            }
                        }
                        (200, response.to_string())
                    })
                }),
            );
            let card_engine = Arc::clone(&engine);
            webhook_hub = webhook_hub.route(
                "/.well-known/agent.json",
                event_route(move |request| {
                    let engine = Arc::clone(&card_engine);
                    Box::pin(async move {
                        if request.method != "GET" {
                            return (405, "method not allowed".to_string());
                        }
                        (200, engine.agent_card().to_string())
                    })
                }),
            );
            platforms.push(a2a);
            webhook_routes += 1;
        }
    }

    // An explicit `gateway enable` with missing credentials is worth saying out loud: the
    // platform stays off, but silence would look like the toggle did nothing.
    for spec in gateway_config::platform_specs() {
        if toggles.toggle(spec.name) == Some(true) {
            let missing = spec.missing();
            if !missing.is_empty() {
                eprintln!(
                    "[gateway] {} is enabled but not configured; missing: {}",
                    spec.name,
                    missing.join(", ")
                );
            }
        }
    }

    if platforms.is_empty() {
        let mut message = format!(
            "no gateway platform is configured\nmissing: {}",
            skipped.join(", ")
        );
        if !disabled.is_empty() {
            message.push_str(&format!("\ndisabled: {}", disabled.join(", ")));
        }
        return Err(message);
    }
    for line in &skipped {
        eprintln!("[gateway] not configured: {line}");
    }
    for name in &disabled {
        eprintln!("[gateway] {name} disabled (vikram gateway enable {name} re-enables)");
    }

    // A pipe has no human at it: approvals are refused, never auto-granted.
    if handle.mode() == PermissionMode::Ask {
        if let Some(session) = handle.snapshot().sessions.first().map(|s| s.id.clone()) {
            handle
                .set_mode(&session, PermissionMode::Unattended)
                .map_err(|err| err.to_string())?;
        }
        eprintln!("[gateway] running unattended; anything needing approval is refused");
    }

    let (stop_tx, stop_rx) = tokio::sync::watch::channel(false);
    // Sessions are keyed by the routing session key: it already encodes the platform, the chat,
    // and — for group chats under the default per-user isolation — the sender, so authorized
    // members of one group no longer share a transcript. Two platforms can still hand out the
    // same chat id, and the platform segment keeps their keys distinct.
    let sessions: Arc<Mutex<HashMap<String, vikram_contracts::SessionId>>> =
        Arc::new(Mutex::new(HashMap::new()));

    // Inbound authorization (access::authorize_source) runs on every event before a turn exists.
    // The pairing store holds the operator-approved senders; if it cannot be opened the gateway
    // still runs — allowlists and GATEWAY_ALLOW_ALL_USERS keep working, only pairing approvals are
    // lost, which is the deny-first direction.
    let pairing = match PairingStore::open(vikram_home().join("pairing")) {
        Ok(store) => Some(Arc::new(store)),
        Err(err) => {
            eprintln!("[gateway] pairing store unavailable: {err}; pairing approvals disabled");
            None
        }
    };
    let auth = Arc::new(IngressAuth {
        env: ProcessEnv,
        pairing,
    });

    if let Some(bind) = webhook_bind {
        if webhook_routes == 0 {
            eprintln!(
                "[gateway] GATEWAY_WEBHOOK_BIND is set but no webhook platform is configured"
            );
        } else {
            let hub: Arc<dyn WebhookHandler> = Arc::new(webhook_hub);
            let stop = stop_rx.clone();
            tokio::spawn(async move {
                if let Err(err) = serve_webhooks(&bind, hub, stop).await {
                    eprintln!("[gateway] webhook ingress stopped: {err}");
                }
            });
        }
    }

    let mut tasks = Vec::new();
    // One transport for inbound attachments, shared by every platform's turn handler. It is built
    // here rather than per turn so the connection pool and the timeouts are reused.
    let media_transport: SharedTransport = ReqwestTransport::new()
        .map_err(|err| format!("could not build the media transport: {err}"))?
        .into_shared();
    // One file-backed delivery ledger shared by every platform: replies survive a restart and a
    // failed send is retried by the sweep instead of being dropped. If the store cannot be opened
    // the gateway still runs on the legacy send-and-forget path — never worse than before.
    let ledger = match DeliveryLedger::open(vikram_home().join("gateway_state.db")) {
        Ok(ledger) => Some(Arc::new(ledger)),
        Err(err) => {
            eprintln!("[gateway] delivery ledger unavailable: {err}; sends are not retried");
            None
        }
    };
    for platform in platforms {
        let handle = handle.clone();
        let sessions = Arc::clone(&sessions);
        let media = Arc::clone(&media_transport);
        let auth = Arc::clone(&auth);
        let ledger = ledger.clone();
        let spec = auth_spec(platform.name());
        let outbound = Outbound {
            platform: Arc::clone(&platform),
            ledger: ledger.clone(),
        };
        let handler: vikram_gateway::host::ReplyHandler = Arc::new(move |event| {
            turn(
                handle.clone(),
                Arc::clone(&sessions),
                media.clone(),
                Arc::clone(&auth),
                spec.clone(),
                outbound.clone(),
                event,
            )
        });
        let stop = stop_rx.clone();
        let name = platform.name().to_string();
        let platform_for_log = Arc::clone(&platform);
        tasks.push(tokio::spawn(async move {
            let stats = serve_with_ledger(platform, handler, stop, ledger.clone()).await;
            let suffix = if stats.errors > 0 {
                format!(", {} error(s)", stats.errors)
            } else {
                String::new()
            };
            eprintln!(
                "[gateway:{name}] stopped after {} event(s), {} repl{}",
                stats.events, stats.replies, suffix
            );
            let _ = platform_for_log;
        }));
    }

    eprintln!(
        "[gateway] serving {} platform(s); Ctrl+C stops",
        tasks.len()
    );
    tokio::select! {
        _ = tokio::signal::ctrl_c() => eprintln!("[gateway] stopping"),
        _ = async {
            for task in tasks {
                let _ = task.await;
            }
        } => {}
    }
    let _ = stop_tx.send(true);
    Ok(())
}

/// The caption a turn falls back to when the operator sent only an attachment.
const IMAGE_ONLY_CAPTION: &str = "[image attachment]";

/// The turn one inbound event becomes.
///
/// Text alone is the common case and stays exactly what it was. When attachments were fetched the
/// message carries them as image blocks next to the caption — or next to [`IMAGE_ONLY_CAPTION`],
/// because a turn whose text is empty would otherwise ask the model to interpret an image it was
/// never pointed at.
fn message_for_event(text: &str, images: Vec<ImageSource>) -> Message {
    if images.is_empty() {
        return Message::user(text);
    }
    let caption = if text.trim().is_empty() {
        IMAGE_ONLY_CAPTION.to_string()
    } else {
        text.to_string()
    };
    Message::user_with_images(caption, images)
}

/// Media blocks an assistant message committed during the turn asks the gateway to send back.
/// Only `Role::Assistant` counts: image blocks on user or tool messages are inbound content and
/// must never be echoed to the chat. The contract (`Block::Image`) already carries them; today no
/// producer emits them on assistant turns, so this is the delivery path waiting for one.
fn assistant_media(message: &Message) -> Vec<ImageSource> {
    if message.role != Role::Assistant {
        return Vec::new();
    }
    message.images().cloned().collect()
}

/// One contract image as an outbound attachment. Inline base64 becomes held bytes (a `data:` URL
/// is not something platform adapters can fetch); a fetch failure or undecodable payload is
/// `None` so the caller can log and skip rather than send a broken verb.
fn outbound_media(source: &ImageSource) -> Option<OutboundMedia> {
    match source {
        ImageSource::Url { url } => Some(OutboundMedia::image_url(url.clone())),
        ImageSource::Data { media_type, data } => {
            let bytes = crate::transforms::base64_decode(data).ok()?;
            Some(
                OutboundMedia::new(MediaKind::Image, GatewayMediaSource::Bytes(bytes))
                    .with_mime(media_type.clone()),
            )
        }
    }
}

/// Deliver the media an assistant turn produced, one ledgered [`OutboundVerb::Media`] each.
/// Ids hang off the event's delivery identity (`{base}:media:{n}`) so a redelivered event dedups
/// exactly like the text reply does; without a platform message id the fallback is unique per
/// event, matching what the host does for text.
async fn deliver_assistant_media(
    platform: &Arc<dyn GatewayPlatform>,
    ledger: Option<&DeliveryLedger>,
    event: &MessageEvent,
    chat_id: &str,
    sources: &[ImageSource],
) {
    if sources.is_empty() {
        return;
    }
    let base = event
        .ledger_message_id
        .clone()
        .or_else(|| event.message_id.clone())
        .unwrap_or_else(|| format!("{}:{chat_id}:{}", platform.name(), now_unix()));
    for (index, source) in sources.iter().enumerate() {
        let Some(media) = outbound_media(source) else {
            eprintln!(
                "[gateway:{}] assistant media could not be encoded; skipped",
                platform.name()
            );
            continue;
        };
        let verb = OutboundVerb::media(media);
        let result = deliver_verb(
            platform,
            ledger,
            &format!("{base}:media:{index}"),
            chat_id,
            &verb,
            None,
        )
        .await;
        if !result.success {
            eprintln!(
                "[gateway:{}] media reply to {chat_id} failed: {}",
                platform.name(),
                result
                    .error
                    .clone()
                    .unwrap_or_else(|| "unknown error".into())
            );
        }
    }
}

/// The line a chat-facing tool ends its output with to ask the reply to carry a native poll:
/// `POLL:{"question":"…","options":["a","b"]}` on its own line. The marker rides the committed
/// tool result the way `IMAGE:` does, but its payload is already the shape
/// [`OutboundVerb::Poll`] sends, so no contract block type is needed: the tool result message
/// itself is the carrier.
const POLL_MARKER_PREFIX: &str = "POLL:";

/// One turn carries at most this many polls; a flooded tool batch must not spam the chat.
const MAX_TURN_POLLS: usize = 4;

/// The line a chat-facing tool ends its output with to ask the reply to carry a native card:
/// `CARD:{...}` on its own line. The payload is already the shape [`OutboundVerb::Card`] sends —
/// provider-native JSON passed through untouched — so, like the poll marker, the tool result
/// message itself is the carrier and no contract block type is needed.
const CARD_MARKER_PREFIX: &str = "CARD:";

/// One turn carries at most this many cards; same flood guard as [`MAX_TURN_POLLS`].
const MAX_TURN_CARDS: usize = 4;

/// Longest `CARD:` payload the gateway will deliver. The marker must stay a single line, so the
/// cap is on the raw JSON text, not the parsed value.
const MAX_CARD_MARKER_BYTES: usize = 16_000;

/// The line a chat-facing tool ends its output with to ask the reply to carry an unsent message
/// preview: `DRAFT:{"text":"…"}` on its own line. The payload is already the shape
/// [`OutboundVerb::Draft`] sends minus the caller-chosen `draft_id`, which the gateway derives
/// from the delivery identity so a retried or redelivered event animates the same preview
/// instead of opening a second one.
const DRAFT_MARKER_PREFIX: &str = "DRAFT:";

/// One turn carries at most this many drafts; same flood guard as [`MAX_TURN_POLLS`].
const MAX_TURN_DRAFTS: usize = 4;

/// Longest `DRAFT:` payload the gateway will deliver; same single-line cap as
/// [`MAX_CARD_MARKER_BYTES`].
const MAX_DRAFT_MARKER_BYTES: usize = 16_000;

/// The line a chat-facing tool ends its output with to ask the reply to react to the inbound
/// message: `REACT:{"emoji":"…","remove":false}` on its own line. Unlike the other markers the
/// payload names no target — the gateway attaches the reaction to the platform message id of
/// the event that triggered the turn, which is the only message a turn can honestly react to.
const REACT_MARKER_PREFIX: &str = "REACT:";

/// One turn carries at most this many reactions; same flood guard as [`MAX_TURN_POLLS`].
const MAX_TURN_REACTIONS: usize = 4;

/// Longest emoji a `REACT:` marker will carry; the tool caps at the same size, and anything
/// larger is not an emoji sequence a platform would accept anyway.
const MAX_REACTION_EMOJI_BYTES: usize = 128;

/// The JSON body of a [`REACT_MARKER_PREFIX`] line.
#[derive(serde::Deserialize)]
struct ReactionMarker {
    emoji: String,
    #[serde(default)]
    remove: bool,
}

/// The JSON body of a [`DRAFT_MARKER_PREFIX`] line.
#[derive(serde::Deserialize)]
struct DraftMarker {
    text: String,
}

/// The JSON body of a [`POLL_MARKER_PREFIX`] line.
#[derive(serde::Deserialize)]
struct PollMarker {
    question: String,
    options: Vec<String>,
}

/// Whether a tool's output may raise a poll. Most builtin tools never qualify: their results
/// carry fetched or computed content (a page, a file, a shell log) where the marker could appear
/// by accident or by injection. Only surfaces whose marker is deliberate qualify — operator-
/// installed code (`mcp.*`, `plugin.execute`) and `message.poll`, a builtin whose entire purpose
/// is emitting the marker.
fn poll_marker_tool(name: &str) -> bool {
    name.starts_with("mcp.") || name == "plugin.execute" || name == "message.poll"
}

/// Whether a tool's output may raise a card. Same deliberate-marker rule as
/// [`poll_marker_tool`]: operator-installed code and `message.card`, the builtin whose entire
/// purpose is emitting the marker.
fn card_marker_tool(name: &str) -> bool {
    name.starts_with("mcp.") || name == "plugin.execute" || name == "message.card"
}

/// Whether a tool's output may raise a draft. Same deliberate-marker rule as
/// [`poll_marker_tool`]: operator-installed code and `message.draft`, the builtin whose entire
/// purpose is emitting the marker.
fn draft_marker_tool(name: &str) -> bool {
    name.starts_with("mcp.") || name == "plugin.execute" || name == "message.draft"
}

/// Whether a tool's output may raise a reaction. Same deliberate-marker rule as
/// [`poll_marker_tool`]: operator-installed code and `message.react`, the builtin whose entire
/// purpose is emitting the marker.
fn react_marker_tool(name: &str) -> bool {
    name.starts_with("mcp.") || name == "plugin.execute" || name == "message.react"
}

/// Every well-formed `POLL:` line in a tool result, as `(question, options)`. A line whose JSON
/// does not describe a sendable poll — a blank question, or fewer than two usable options once
/// blanks are dropped and duplicates collapsed — is skipped rather than pushed onto a platform
/// that would refuse it anyway.
fn poll_markers(content: &str) -> Vec<(String, Vec<String>)> {
    let mut polls = Vec::new();
    for line in content.lines() {
        let Some(raw) = line.strip_prefix(POLL_MARKER_PREFIX) else {
            continue;
        };
        let Ok(marker) = serde_json::from_str::<PollMarker>(raw.trim()) else {
            continue;
        };
        let question = marker.question.trim();
        if question.is_empty() {
            continue;
        }
        let mut options: Vec<String> = Vec::new();
        for option in marker.options {
            let option = option.trim();
            if !option.is_empty() && !options.iter().any(|seen| seen == option) {
                options.push(option.to_owned());
            }
        }
        if options.len() < 2 {
            continue;
        }
        polls.push((question.to_owned(), options));
    }
    polls
}

/// Every well-formed `CARD:` line in a tool result, as the card payload. A line whose JSON is
/// not an object — or is too large to have come from a marker — is skipped rather than pushed
/// onto a platform that would refuse it anyway.
fn card_markers(content: &str) -> Vec<serde_json::Value> {
    let mut cards = Vec::new();
    for line in content.lines() {
        let Some(raw) = line.strip_prefix(CARD_MARKER_PREFIX) else {
            continue;
        };
        let raw = raw.trim();
        if raw.len() > MAX_CARD_MARKER_BYTES {
            continue;
        }
        let Ok(payload) = serde_json::from_str::<serde_json::Value>(raw) else {
            continue;
        };
        if payload.as_object().is_some_and(|map| !map.is_empty()) {
            cards.push(payload);
        }
    }
    cards
}

/// Every well-formed `DRAFT:` line in a tool result, as draft texts. A line whose JSON does not
/// describe a sendable draft — not an object with a `text` string, blank text, or a payload too
/// large to have come from a marker — is skipped rather than pushed onto a platform that would
/// refuse it anyway.
fn draft_markers(content: &str) -> Vec<String> {
    let mut drafts = Vec::new();
    for line in content.lines() {
        let Some(raw) = line.strip_prefix(DRAFT_MARKER_PREFIX) else {
            continue;
        };
        let raw = raw.trim();
        if raw.len() > MAX_DRAFT_MARKER_BYTES {
            continue;
        }
        let Ok(marker) = serde_json::from_str::<DraftMarker>(raw) else {
            continue;
        };
        if marker.text.trim().is_empty() {
            continue;
        }
        drafts.push(marker.text);
    }
    drafts
}

/// Every well-formed `REACT:` line in a tool result, as `(emoji, remove)` pairs. A line whose
/// JSON does not describe a sendable reaction — a missing or blank emoji, or one too large to
/// be an emoji sequence — is skipped rather than pushed onto a platform that would refuse it
/// anyway.
fn reaction_markers(content: &str) -> Vec<(String, bool)> {
    let mut reactions = Vec::new();
    for line in content.lines() {
        let Some(raw) = line.strip_prefix(REACT_MARKER_PREFIX) else {
            continue;
        };
        let Ok(marker) = serde_json::from_str::<ReactionMarker>(raw.trim()) else {
            continue;
        };
        let emoji = marker.emoji.trim();
        if emoji.is_empty() || emoji.len() > MAX_REACTION_EMOJI_BYTES {
            continue;
        }
        reactions.push((emoji.to_owned(), marker.remove));
    }
    reactions
}

/// The polls, cards, drafts and reactions a turn's committed messages asked the gateway to send. Assistant
/// messages carry the `ToolCall` blocks naming each call; tool messages carry the `ToolResult`
/// content the markers ride in. Both arrive through `MessageCommitted` call-then-result, so the
/// name map is always populated before the result that needs it.
#[derive(Default)]
struct TurnMarkers {
    names: HashMap<ToolCallId, String>,
    polls: Vec<(String, Vec<String>)>,
    cards: Vec<serde_json::Value>,
    drafts: Vec<String>,
    reactions: Vec<(String, bool)>,
}

impl TurnMarkers {
    fn observe(&mut self, message: &Message) {
        match message.role {
            Role::Assistant => {
                for block in &message.blocks {
                    if let Block::ToolCall { call } = block {
                        self.names.insert(call.id.clone(), call.name.clone());
                    }
                }
            }
            Role::Tool => {
                for block in &message.blocks {
                    let Block::ToolResult {
                        call, content, ok, ..
                    } = block
                    else {
                        continue;
                    };
                    if !*ok {
                        continue;
                    }
                    let Some(name) = self.names.get(call) else {
                        continue;
                    };
                    if self.polls.len() < MAX_TURN_POLLS && poll_marker_tool(name) {
                        for poll in poll_markers(content) {
                            if self.polls.len() >= MAX_TURN_POLLS {
                                break;
                            }
                            self.polls.push(poll);
                        }
                    }
                    if self.cards.len() < MAX_TURN_CARDS && card_marker_tool(name) {
                        for card in card_markers(content) {
                            if self.cards.len() >= MAX_TURN_CARDS {
                                break;
                            }
                            self.cards.push(card);
                        }
                    }
                    if self.drafts.len() < MAX_TURN_DRAFTS && draft_marker_tool(name) {
                        for draft in draft_markers(content) {
                            if self.drafts.len() >= MAX_TURN_DRAFTS {
                                break;
                            }
                            self.drafts.push(draft);
                        }
                    }
                    if self.reactions.len() < MAX_TURN_REACTIONS && react_marker_tool(name) {
                        for reaction in reaction_markers(content) {
                            if self.reactions.len() >= MAX_TURN_REACTIONS {
                                break;
                            }
                            self.reactions.push(reaction);
                        }
                    }
                }
            }
            _ => {}
        }
    }
}

/// Deliver the polls a turn produced, one ledgered [`OutboundVerb::Poll`] each. Ids hang off
/// the event's delivery identity (`{base}:poll:{n}`) so a redelivered event dedups exactly
/// like the text reply and the media verbs do.
async fn deliver_assistant_polls(
    platform: &Arc<dyn GatewayPlatform>,
    ledger: Option<&DeliveryLedger>,
    event: &MessageEvent,
    chat_id: &str,
    polls: &[(String, Vec<String>)],
) {
    if polls.is_empty() {
        return;
    }
    let base = event
        .ledger_message_id
        .clone()
        .or_else(|| event.message_id.clone())
        .unwrap_or_else(|| format!("{}:{chat_id}:{}", platform.name(), now_unix()));
    for (index, (question, options)) in polls.iter().enumerate() {
        let verb = OutboundVerb::poll(question.clone(), options.clone());
        let result = deliver_verb(
            platform,
            ledger,
            &format!("{base}:poll:{index}"),
            chat_id,
            &verb,
            None,
        )
        .await;
        if !result.success {
            eprintln!(
                "[gateway:{}] poll reply to {chat_id} failed: {}",
                platform.name(),
                result
                    .error
                    .clone()
                    .unwrap_or_else(|| "unknown error".into())
            );
        }
    }
}

/// Deliver the cards a turn produced, one ledgered [`OutboundVerb::Card`] each. Ids hang off
/// the event's delivery identity (`{base}:card:{n}`) so a redelivered event dedups exactly
/// like the text reply, the media verbs and the polls do. A platform without card support
/// refuses the verb honestly through [`PlatformAdapter::send_card`]'s default.
async fn deliver_assistant_cards(
    platform: &Arc<dyn GatewayPlatform>,
    ledger: Option<&DeliveryLedger>,
    event: &MessageEvent,
    chat_id: &str,
    cards: &[serde_json::Value],
) {
    if cards.is_empty() {
        return;
    }
    let base = event
        .ledger_message_id
        .clone()
        .or_else(|| event.message_id.clone())
        .unwrap_or_else(|| format!("{}:{chat_id}:{}", platform.name(), now_unix()));
    for (index, payload) in cards.iter().enumerate() {
        let verb = OutboundVerb::card(payload.clone());
        let result = deliver_verb(
            platform,
            ledger,
            &format!("{base}:card:{index}"),
            chat_id,
            &verb,
            None,
        )
        .await;
        if !result.success {
            eprintln!(
                "[gateway:{}] card reply to {chat_id} failed: {}",
                platform.name(),
                result
                    .error
                    .clone()
                    .unwrap_or_else(|| "unknown error".into())
            );
        }
    }
}

/// A stable `u64` draft id for one ledgered draft delivery. [`OutboundVerb::Draft`]'s id is
/// chosen by the caller — platforms that stream drafts animate the same preview for the same id
/// — so hashing the delivery identity keeps ledger retries and event redeliveries on one
/// preview while distinct markers still open distinct ones.
fn draft_id_for(delivery_id: &str) -> u64 {
    let mut hash = 0xcbf29ce484222325u64;
    for byte in delivery_id.as_bytes() {
        hash ^= u64::from(*byte);
        hash = hash.wrapping_mul(0x100000001b3);
    }
    hash
}

/// Deliver the drafts a turn produced, one ledgered [`OutboundVerb::Draft`] each. Ids hang off
/// the event's delivery identity (`{base}:draft:{n}`) so a redelivered event dedups exactly
/// like the text reply, the media verbs, the polls and the cards do; the verb's `draft_id` is
/// derived from that same identity so a retry animates the same preview. A platform without
/// draft-streaming support refuses the verb honestly through
/// [`PlatformAdapter::send_draft`]'s default.
async fn deliver_assistant_drafts(
    platform: &Arc<dyn GatewayPlatform>,
    ledger: Option<&DeliveryLedger>,
    event: &MessageEvent,
    chat_id: &str,
    drafts: &[String],
) {
    if drafts.is_empty() {
        return;
    }
    let base = event
        .ledger_message_id
        .clone()
        .or_else(|| event.message_id.clone())
        .unwrap_or_else(|| format!("{}:{chat_id}:{}", platform.name(), now_unix()));
    for (index, text) in drafts.iter().enumerate() {
        let delivery_id = format!("{base}:draft:{index}");
        let verb = OutboundVerb::draft(draft_id_for(&delivery_id), text.clone());
        let result = deliver_verb(platform, ledger, &delivery_id, chat_id, &verb, None).await;
        if !result.success {
            eprintln!(
                "[gateway:{}] draft reply to {chat_id} failed: {}",
                platform.name(),
                result
                    .error
                    .clone()
                    .unwrap_or_else(|| "unknown error".into())
            );
        }
    }
}

/// Deliver the reactions a turn produced, one ledgered [`OutboundVerb::Reaction`] each. The
/// target is the platform message id of the event that triggered the turn (`message_id`, with
/// `source.message_id` as the fallback some adapters only fill there); an event that carried
/// no message id cannot be reacted to, so the markers are dropped with a log line rather than
/// aimed at a guessed message. Ids hang off the event's delivery identity
/// (`{base}:react:{n}`) so a redelivered event dedups exactly like the text reply and the
/// other verbs do. A platform without reaction support refuses the verb honestly through
/// [`PlatformAdapter::add_reaction`]'s default.
async fn deliver_assistant_reactions(
    platform: &Arc<dyn GatewayPlatform>,
    ledger: Option<&DeliveryLedger>,
    event: &MessageEvent,
    chat_id: &str,
    reactions: &[(String, bool)],
) {
    if reactions.is_empty() {
        return;
    }
    let Some(target) = event
        .message_id
        .clone()
        .or_else(|| event.source.message_id.clone())
    else {
        eprintln!(
            "[gateway:{}] reaction markers dropped: event carried no message id",
            platform.name()
        );
        return;
    };
    let base = event
        .ledger_message_id
        .clone()
        .or_else(|| event.message_id.clone())
        .unwrap_or_else(|| format!("{}:{chat_id}:{}", platform.name(), now_unix()));
    for (index, (emoji, remove)) in reactions.iter().enumerate() {
        let verb = if *remove {
            OutboundVerb::remove_reaction(target.clone(), emoji.clone())
        } else {
            OutboundVerb::reaction(target.clone(), emoji.clone())
        };
        let result = deliver_verb(
            platform,
            ledger,
            &format!("{base}:react:{index}"),
            chat_id,
            &verb,
            None,
        )
        .await;
        if !result.success {
            eprintln!(
                "[gateway:{}] reaction on {chat_id} failed: {}",
                platform.name(),
                result
                    .error
                    .clone()
                    .unwrap_or_else(|| "unknown error".into())
            );
        }
    }
}

/// What a turn needs to deliver non-text outbound verbs itself: the platform to send through
/// and the ledger that dedups and retries the send. The text reply still returns through the
/// host's own path.
#[derive(Clone)]
struct Outbound {
    platform: Arc<dyn GatewayPlatform>,
    ledger: Option<Arc<DeliveryLedger>>,
}

/// One inbound event → one runtime turn → the turn's text.
///
/// Authorization runs first: `access::authorize_source` decides whether this sender may talk to
/// the agent at all, and an unauthorized event is dropped before a session exists or a single
/// attachment is fetched. Synthetic `internal` events (the raft wake bridge) are exempt — they are
/// produced inside this process, not by a chat peer.
fn turn(
    handle: RuntimeHandle,
    sessions: Arc<Mutex<HashMap<String, vikram_contracts::SessionId>>>,
    media_transport: SharedTransport,
    auth: Arc<IngressAuth>,
    spec: PlatformAuthSpec,
    outbound: Outbound,
    event: MessageEvent,
) -> vikram_gateway::host::ReplyFuture {
    Box::pin(async move {
        if !event.internal {
            let input = AuthzInput {
                source: &event.source,
                role_authorized: event.source.role_authorized,
                is_bot: event.source.is_bot,
                spec: &spec,
                allow_adapter_delegation: true,
            };
            let pairing = auth
                .pairing
                .as_deref()
                .map(|store| store as &dyn PairingVerdict);
            if !authorize_source(&input, &auth.env, pairing, None) {
                eprintln!(
                    "[gateway:{}] dropped message from unauthorized sender (chat {}, user {:?})",
                    event.source.platform, event.source.chat_id, event.source.user_id
                );
                return None;
            }
        }
        let chat = event.source.chat_id.clone();
        let session_key = build_session_key(
            &event.source,
            &SessionKeyOptions::default(),
            event.source.profile.as_deref(),
        );
        let existing = sessions
            .lock()
            .ok()
            .and_then(|map| map.get(&session_key).cloned());
        let session = match existing {
            Some(session) => session,
            None => {
                let title = format!("{} · {}", event.source.platform, chat);
                let session = handle.create_session(Some(title)).ok()?;
                if let Ok(mut map) = sessions.lock() {
                    map.insert(session_key, session.clone());
                }
                session
            }
        };
        // Attachments are fetched before the turn is submitted, because the runtime takes a whole
        // message and cannot be handed the images later. A fetch that fails or is refused only
        // costs the attachment: the text still goes through, and the reason is logged (never the
        // URL — an attachment URL is usually a signed, expiring one).
        let mut images = Vec::new();
        if !event.media_urls.is_empty() {
            let (fetched, skipped) = fetch_images_with_skips(
                media_transport.as_ref(),
                &event.media_urls,
                &event.media_types,
                &MediaFetchLimits::default(),
            )
            .await;
            for reason in &skipped {
                eprintln!(
                    "[gateway:{}] attachment skipped: {reason}",
                    event.source.platform
                );
            }
            images = fetched
                .into_iter()
                .map(|image| image.into_source())
                .collect();
        }
        let carried_images = !images.is_empty();
        let message = message_for_event(&event.text, images);
        let mut events = handle.events();
        if let Err(err) = handle.submit_message(&session, message) {
            // The runtime refuses an attachment behind a busy session rather than running a turn
            // without it. The gateway has a better option than dropping the turn: send the text
            // on its own, which is exactly what it did before attachments existed.
            if !carried_images {
                return None;
            }
            eprintln!(
                "[gateway:{}] attachment could not be submitted ({err}); sending the text only",
                event.source.platform
            );
            if handle.submit(&session, event.text.clone()).is_err() {
                return None;
            }
        }
        let mut reply = String::new();
        // Assistant messages committed mid-turn can carry media blocks; they are collected here
        // and sent as verbs once the turn's text is known not to be a lagged prefix.
        let mut reply_media: Vec<ImageSource> = Vec::new();
        // Tool results committed mid-turn can carry `POLL:`/`CARD:`/`DRAFT:`/`REACT:` markers; they are
        // collected here and sent as verbs once the turn's text is known not to be a lagged
        // prefix.
        let mut turn_markers = TurnMarkers::default();
        // A lagged receiver skipped events, so what was collected may be a prefix of the real
        // reply. Keep draining for the turn's end (bounded — a flooded channel must not spin
        // forever), but never send the partial text as if it were the final answer.
        let mut incomplete = false;
        let mut lags = 0u32;
        loop {
            match events.recv().await {
                Ok(RuntimeEvent::StreamDelta {
                    session: sid, text, ..
                }) if sid == session => {
                    reply.push_str(&text);
                    lags = 0;
                }
                Ok(RuntimeEvent::MessageCommitted {
                    session: sid,
                    message,
                }) if sid == session => {
                    reply_media.extend(assistant_media(&message));
                    turn_markers.observe(&message);
                    lags = 0;
                }
                Ok(RuntimeEvent::TurnFinished {
                    session: sid,
                    state,
                    ..
                }) if sid == session => {
                    if matches!(
                        state,
                        vikram_runtime::types::TurnState::Failed
                            | vikram_runtime::types::TurnState::Cancelled
                    ) {
                        return None;
                    }
                    break;
                }
                Ok(RuntimeEvent::Shutdown) => break,
                Ok(_) => {
                    lags = 0;
                }
                Err(tokio::sync::broadcast::error::RecvError::Lagged(_)) => {
                    incomplete = true;
                    lags += 1;
                    if lags > 32 {
                        eprintln!(
                            "[gateway:{}] event stream stayed lagged; dropping the reply",
                            event.source.platform
                        );
                        return None;
                    }
                }
                Err(tokio::sync::broadcast::error::RecvError::Closed) => break,
            }
        }
        if incomplete {
            eprintln!(
                "[gateway:{}] reply for {} was truncated by event lag; not sent",
                event.source.platform, chat
            );
            return None;
        }
        deliver_assistant_media(
            &outbound.platform,
            outbound.ledger.as_deref(),
            &event,
            &chat,
            &reply_media,
        )
        .await;
        deliver_assistant_polls(
            &outbound.platform,
            outbound.ledger.as_deref(),
            &event,
            &chat,
            &turn_markers.polls,
        )
        .await;
        deliver_assistant_cards(
            &outbound.platform,
            outbound.ledger.as_deref(),
            &event,
            &chat,
            &turn_markers.cards,
        )
        .await;
        deliver_assistant_drafts(
            &outbound.platform,
            outbound.ledger.as_deref(),
            &event,
            &chat,
            &turn_markers.drafts,
        )
        .await;
        deliver_assistant_reactions(
            &outbound.platform,
            outbound.ledger.as_deref(),
            &event,
            &chat,
            &turn_markers.reactions,
        )
        .await;
        if reply.trim().is_empty() {
            None
        } else {
            Some(reply)
        }
    })
}

fn url_decode(value: &str) -> String {
    let bytes = value.as_bytes();
    let mut out = Vec::with_capacity(bytes.len());
    let mut index = 0;
    while index < bytes.len() {
        match bytes[index] {
            b'+' => {
                out.push(b' ');
                index += 1;
            }
            b'%' if index + 2 < bytes.len() => {
                let hex = std::str::from_utf8(&bytes[index + 1..index + 3]).unwrap_or("");
                match u8::from_str_radix(hex, 16) {
                    Ok(byte) => {
                        out.push(byte);
                        index += 3;
                    }
                    Err(_) => {
                        out.push(b'%');
                        index += 1;
                    }
                }
            }
            byte => {
                out.push(byte);
                index += 1;
            }
        }
    }
    String::from_utf8_lossy(&out).into_owned()
}

fn parse_pairs(raw: &str) -> Vec<(String, String)> {
    raw.split('&')
        .filter(|pair| !pair.is_empty())
        .map(|pair| match pair.split_once('=') {
            Some((key, value)) => (url_decode(key), url_decode(value)),
            None => (url_decode(pair), String::new()),
        })
        .collect()
}

fn parse_query(path: &str) -> Vec<(String, String)> {
    match path.split_once('?') {
        Some((_, query)) => parse_pairs(query),
        None => Vec::new(),
    }
}

fn parse_form(body: &str) -> Vec<(String, String)> {
    parse_pairs(body)
}

fn query_value<'a>(pairs: &'a [(String, String)], key: &str) -> Option<&'a str> {
    pairs
        .iter()
        .find(|(name, _)| name == key)
        .map(|(_, value)| value.as_str())
}

#[cfg(test)]
mod tests {
    use super::*;
    use vikram_contracts::{ToolCall, Trust};

    #[test]
    fn configuration_names_are_read_from_the_environment() {
        // The helper trims and rejects empty values; exercising it avoids depending on process env.
        assert_eq!(env("VIKRAM_GATEWAY_TEST_UNSET_VARIABLE"), None);
    }

    #[test]
    fn a_text_only_event_becomes_a_text_turn() {
        let message = message_for_event("why is the build red?", Vec::new());
        assert_eq!(message, Message::user("why is the build red?"));
        assert_eq!(message.images().count(), 0);
        assert_eq!(message.min_trust(), Some(Trust::User));
    }

    #[test]
    fn an_event_with_images_keeps_its_caption_beside_them() {
        let image = ImageSource::data("image/png", "iVBORw0KGgo=");
        let message = message_for_event("what is wrong with this window?", vec![image.clone()]);
        assert_eq!(message.text_body(), "what is wrong with this window?");
        assert_eq!(message.images().collect::<Vec<_>>(), vec![&image]);
        // An attachment is external content: it can inform the model, never authorize an action.
        assert_eq!(message.min_trust(), Some(Trust::External));
        assert!(message.carries_untrusted_content());
    }

    #[test]
    fn a_blank_caption_falls_back_to_the_image_placeholder() {
        for blank in ["", "   ", "\n\t"] {
            let message = message_for_event(blank, vec![ImageSource::data("image/jpeg", "AAAA")]);
            assert_eq!(message.text_body(), IMAGE_ONLY_CAPTION, "{blank:?}");
            assert_eq!(message.images().count(), 1, "{blank:?}");
        }
    }

    fn assistant_with_image(source: ImageSource) -> Message {
        Message {
            role: Role::Assistant,
            blocks: vec![vikram_contracts::Block::Image {
                source,
                trust: Trust::Model,
            }],
        }
    }

    #[test]
    fn only_assistant_messages_yield_outbound_media() {
        let image = ImageSource::url("https://example.test/out.png");
        let assistant = assistant_with_image(image.clone());
        assert_eq!(assistant_media(&assistant), vec![image]);

        // The same block on a user or tool message is inbound content, never echoed back.
        for role in [Role::User, Role::Tool, Role::System] {
            let mut message = assistant_with_image(ImageSource::url("https://x/y.png"));
            message.role = role;
            assert!(assistant_media(&message).is_empty(), "{role:?}");
        }
        assert!(assistant_media(&Message::user("plain")).is_empty());
    }

    #[test]
    fn a_url_image_becomes_a_media_verb_with_a_fetchable_source() {
        let media = outbound_media(&ImageSource::url("https://example.test/a.png"))
            .expect("url images always convert");
        assert_eq!(media.kind, MediaKind::Image);
        assert_eq!(
            media.source,
            GatewayMediaSource::Url("https://example.test/a.png".into())
        );
    }

    #[test]
    fn inline_image_bytes_become_held_media_with_their_mime() {
        // "hello" in base64.
        let media = outbound_media(&ImageSource::data("image/png", "aGVsbG8="))
            .expect("valid base64 converts");
        assert_eq!(media.kind, MediaKind::Image);
        assert_eq!(media.mime.as_deref(), Some("image/png"));
        assert_eq!(media.source, GatewayMediaSource::Bytes(b"hello".to_vec()));
    }

    #[test]
    fn undecodable_inline_media_is_refused_not_sent_broken() {
        assert!(outbound_media(&ImageSource::data("image/png", "!!!not-base64!!!")).is_none());
        assert!(outbound_media(&ImageSource::data("image/png", "a")).is_none());
    }

    /// A platform that records every verb `send_verb` receives; the text path stays a plain
    /// `send` so a regression that bypasses verb dispatch is visible.
    #[derive(Default)]
    struct RecordingPlatform {
        verbs: Mutex<Vec<OutboundVerb>>,
    }

    #[async_trait::async_trait]
    impl GatewayPlatform for RecordingPlatform {
        fn name(&self) -> &str {
            "rec"
        }

        async fn next_events(&self) -> Result<Vec<MessageEvent>, String> {
            Ok(Vec::new())
        }

        async fn send(
            &self,
            _chat_id: &str,
            _content: &str,
        ) -> vikram_gateway::platform::SendResult {
            vikram_gateway::platform::SendResult::ok("text-id")
        }

        async fn send_verb(
            &self,
            _chat_id: &str,
            verb: &OutboundVerb,
            _reply_to: Option<&str>,
        ) -> vikram_gateway::platform::SendResult {
            self.verbs.lock().unwrap().push(verb.clone());
            vikram_gateway::platform::SendResult::ok("media-id")
        }
    }

    fn test_event() -> MessageEvent {
        let source = vikram_gateway::event::SessionSource::new(
            vikram_gateway::event::Platform::new("rec").expect("valid platform"),
            "chat-1",
        );
        let mut event = MessageEvent::new("look", source);
        event.message_id = Some("m-1".to_string());
        event
    }

    #[tokio::test]
    async fn assistant_media_is_delivered_as_ledgered_media_verbs() {
        let platform = Arc::new(RecordingPlatform::default());
        let dyn_platform: Arc<dyn GatewayPlatform> = platform.clone();
        let ledger = DeliveryLedger::in_memory().expect("in-memory ledger");
        let event = test_event();
        let sources = vec![
            ImageSource::url("https://example.test/a.png"),
            ImageSource::data("image/png", "aGVsbG8="),
        ];

        deliver_assistant_media(&dyn_platform, Some(&ledger), &event, "chat-1", &sources).await;
        // A redelivery of the same event must not enqueue or send a second copy.
        deliver_assistant_media(&dyn_platform, Some(&ledger), &event, "chat-1", &sources).await;

        let verbs = platform.verbs.lock().unwrap();
        assert_eq!(verbs.len(), 2, "each image becomes exactly one media verb");
        for verb in verbs.iter() {
            let OutboundVerb::Media { media } = verb else {
                panic!("expected a media verb, got {verb:?}");
            };
            assert_eq!(media.kind, MediaKind::Image);
        }
        assert_eq!(ledger.len().expect("ledger len"), 2);
    }

    #[tokio::test]
    async fn text_only_turns_send_no_verbs() {
        let platform = Arc::new(RecordingPlatform::default());
        let dyn_platform: Arc<dyn GatewayPlatform> = platform.clone();
        let event = test_event();
        deliver_assistant_media(&dyn_platform, None, &event, "chat-1", &[]).await;
        assert!(platform.verbs.lock().unwrap().is_empty());
    }

    #[test]
    fn a_poll_marker_line_parses_to_question_and_options() {
        let content = "created the survey\nPOLL:{\"question\":\"Lunch?\",\"options\":[\"Pizza\",\"Sushi\"]}\ndone";
        assert_eq!(
            poll_markers(content),
            vec![(
                "Lunch?".to_owned(),
                vec!["Pizza".to_owned(), "Sushi".to_owned()]
            )]
        );
    }

    #[test]
    fn malformed_or_unsendable_poll_markers_are_skipped() {
        for content in [
            "no marker here",
            "POLL:{not json}",
            "POLL:{\"question\":\"  \",\"options\":[\"a\",\"b\"]}",
            "POLL:{\"question\":\"Pick?\",\"options\":[\"only\"]}",
            "POLL:{\"question\":\"Pick?\",\"options\":[\"\",\" \"]}",
            "POLL:{\"question\":\"Pick?\"}",
        ] {
            assert!(poll_markers(content).is_empty(), "{content}");
        }
        // Blank and duplicate options collapse before the two-option floor is checked.
        let content = "POLL:{\"question\":\"Pick?\",\"options\":[\" a \",\"a\",\"\",\"b\"]}";
        assert_eq!(
            poll_markers(content),
            vec![("Pick?".to_owned(), vec!["a".to_owned(), "b".to_owned()])]
        );
    }

    fn tool_call_message(name: &str) -> (Message, ToolCallId) {
        let call = ToolCall::new(name, serde_json::json!({}));
        let id = call.id.clone();
        (
            Message {
                role: Role::Assistant,
                blocks: vec![Block::ToolCall { call }],
            },
            id,
        )
    }

    fn tool_result_message(call: ToolCallId, content: &str, ok: bool) -> Message {
        Message {
            role: Role::Tool,
            blocks: vec![Block::ToolResult {
                call,
                content: content.to_owned(),
                trust: Trust::External,
                ok,
            }],
        }
    }

    #[test]
    fn only_allowlisted_tools_raise_polls_from_their_results() {
        let mut polls = TurnMarkers::default();
        let marker = "POLL:{\"question\":\"Lunch?\",\"options\":[\"Pizza\",\"Sushi\"]}";

        // An MCP tool's marker is honored once its call is known.
        let (call_msg, mcp_id) = tool_call_message("mcp.survey.create");
        polls.observe(&call_msg);
        polls.observe(&tool_result_message(mcp_id, marker, true));
        assert_eq!(polls.polls.len(), 1);
        assert_eq!(polls.polls[0].0, "Lunch?");

        // Builtin tools — even ones whose output is operator-influenced — never qualify:
        // fetched pages and shell logs can print the marker by accident or by injection.
        for name in [
            "web.fetch",
            "web.search",
            "shell.run",
            "fs.read",
            "plugin.list",
        ] {
            let (call_msg, id) = tool_call_message(name);
            polls.observe(&call_msg);
            polls.observe(&tool_result_message(id, marker, true));
        }
        // `plugin.execute` output is operator-installed plugin code, so it does qualify.
        let (call_msg, plugin_id) = tool_call_message("plugin.execute");
        polls.observe(&call_msg);
        polls.observe(&tool_result_message(plugin_id, marker, true));
        assert_eq!(polls.polls.len(), 2);

        // `message.poll` is the one builtin whose whole purpose is emitting the marker.
        let (call_msg, poll_id) = tool_call_message("message.poll");
        polls.observe(&call_msg);
        polls.observe(&tool_result_message(poll_id, marker, true));
        assert_eq!(polls.polls.len(), 3);

        // A failed result and an unknown call id raise nothing.
        let (call_msg, failed_id) = tool_call_message("mcp.survey.create");
        polls.observe(&call_msg);
        polls.observe(&tool_result_message(failed_id, marker, false));
        polls.observe(&tool_result_message(ToolCallId::generate(), marker, true));
        assert_eq!(polls.polls.len(), 3);
    }

    #[test]
    fn a_turn_carries_at_most_the_poll_cap() {
        let mut polls = TurnMarkers::default();
        let (call_msg, id) = tool_call_message("mcp.survey.create");
        polls.observe(&call_msg);
        let marker = "POLL:{\"question\":\"Q?\",\"options\":[\"a\",\"b\"]}";
        for _ in 0..MAX_TURN_POLLS + 2 {
            polls.observe(&tool_result_message(id.clone(), marker, true));
        }
        assert_eq!(polls.polls.len(), MAX_TURN_POLLS);
    }

    #[tokio::test]
    async fn assistant_polls_are_delivered_as_ledgered_poll_verbs() {
        let platform = Arc::new(RecordingPlatform::default());
        let dyn_platform: Arc<dyn GatewayPlatform> = platform.clone();
        let ledger = DeliveryLedger::in_memory().expect("in-memory ledger");
        let event = test_event();
        let polls = vec![
            (
                "Lunch?".to_owned(),
                vec!["Pizza".to_owned(), "Sushi".to_owned()],
            ),
            (
                "Ship it?".to_owned(),
                vec!["yes".to_owned(), "no".to_owned()],
            ),
        ];

        deliver_assistant_polls(&dyn_platform, Some(&ledger), &event, "chat-1", &polls).await;
        // A redelivery of the same event must not enqueue or send a second copy.
        deliver_assistant_polls(&dyn_platform, Some(&ledger), &event, "chat-1", &polls).await;

        let verbs = platform.verbs.lock().unwrap();
        assert_eq!(verbs.len(), 2, "each poll becomes exactly one poll verb");
        for verb in verbs.iter() {
            let OutboundVerb::Poll { question, options } = verb else {
                panic!("expected a poll verb, got {verb:?}");
            };
            assert!(!question.is_empty());
            assert!(options.len() >= 2);
        }
        assert_eq!(ledger.len().expect("ledger len"), 2);
    }

    #[tokio::test]
    async fn turns_without_polls_send_no_poll_verbs() {
        let platform = Arc::new(RecordingPlatform::default());
        let dyn_platform: Arc<dyn GatewayPlatform> = platform.clone();
        let event = test_event();
        deliver_assistant_polls(&dyn_platform, None, &event, "chat-1", &[]).await;
        assert!(platform.verbs.lock().unwrap().is_empty());
    }

    #[test]
    fn a_card_marker_line_parses_to_its_payload() {
        let content = "built the summary\nCARD:{\"blocks\":[{\"type\":\"section\",\"text\":{\"type\":\"mrkdwn\",\"text\":\"hi\"}}]}\ndone";
        let cards = card_markers(content);
        assert_eq!(cards.len(), 1);
        assert_eq!(cards[0]["blocks"][0]["type"], "section");
    }

    #[test]
    fn malformed_or_oversized_card_markers_are_skipped() {
        for content in [
            "no marker here",
            "CARD:{not json}",
            "CARD:[1,2,3]",
            "CARD:\"just a string\"",
            "CARD:42",
            "CARD:null",
            "CARD:{}",
            "CARD:{} trailing",
        ] {
            assert!(card_markers(content).is_empty(), "{content}");
        }
        // A payload over the byte cap is skipped even though it is valid JSON.
        let huge = format!("CARD:{{\"pad\":\"{}\"}}", "x".repeat(MAX_CARD_MARKER_BYTES));
        assert!(card_markers(&huge).is_empty());
        // A marker embedded mid-line is not a marker.
        assert!(card_markers("prefix CARD:{\"a\":1}").is_empty());
    }

    #[test]
    fn only_allowlisted_tools_raise_cards_from_their_results() {
        let mut markers = TurnMarkers::default();
        let marker = "CARD:{\"type\":\"AdaptiveCard\",\"body\":[]}";

        // An MCP tool's marker is honored once its call is known.
        let (call_msg, mcp_id) = tool_call_message("mcp.cards.send");
        markers.observe(&call_msg);
        markers.observe(&tool_result_message(mcp_id, marker, true));
        assert_eq!(markers.cards.len(), 1);

        // Builtin tools whose output is fetched or computed content never qualify.
        for name in [
            "web.fetch",
            "shell.run",
            "fs.read",
            "plugin.list",
            "message.poll",
        ] {
            let (call_msg, id) = tool_call_message(name);
            markers.observe(&call_msg);
            markers.observe(&tool_result_message(id, marker, true));
        }
        // `plugin.execute` output is operator-installed plugin code, so it does qualify.
        let (call_msg, plugin_id) = tool_call_message("plugin.execute");
        markers.observe(&call_msg);
        markers.observe(&tool_result_message(plugin_id, marker, true));
        assert_eq!(markers.cards.len(), 2);

        // `message.card` is the one builtin whose whole purpose is emitting the marker.
        let (call_msg, card_id) = tool_call_message("message.card");
        markers.observe(&call_msg);
        markers.observe(&tool_result_message(card_id, marker, true));
        assert_eq!(markers.cards.len(), 3);

        // A failed result and an unknown call id raise nothing.
        let (call_msg, failed_id) = tool_call_message("mcp.cards.send");
        markers.observe(&call_msg);
        markers.observe(&tool_result_message(failed_id, marker, false));
        markers.observe(&tool_result_message(ToolCallId::generate(), marker, true));
        assert_eq!(markers.cards.len(), 3);
    }

    #[test]
    fn poll_and_card_markers_are_gated_independently() {
        let mut markers = TurnMarkers::default();
        // `message.poll` may raise polls but not cards; `message.card` the reverse.
        let poll_marker = "POLL:{\"question\":\"Q?\",\"options\":[\"a\",\"b\"]}";
        let card_marker = "CARD:{\"type\":\"AdaptiveCard\"}";

        let (call_msg, poll_id) = tool_call_message("message.poll");
        markers.observe(&call_msg);
        markers.observe(&tool_result_message(
            poll_id,
            &format!("{poll_marker}\n{card_marker}"),
            true,
        ));
        assert_eq!(markers.polls.len(), 1);
        assert_eq!(markers.cards.len(), 0);

        let (call_msg, card_id) = tool_call_message("message.card");
        markers.observe(&call_msg);
        markers.observe(&tool_result_message(
            card_id,
            &format!("{poll_marker}\n{card_marker}"),
            true,
        ));
        assert_eq!(markers.polls.len(), 1);
        assert_eq!(markers.cards.len(), 1);
    }

    #[test]
    fn a_turn_carries_at_most_the_card_cap() {
        let mut markers = TurnMarkers::default();
        let (call_msg, id) = tool_call_message("mcp.cards.send");
        markers.observe(&call_msg);
        let marker = "CARD:{\"type\":\"AdaptiveCard\"}";
        for _ in 0..MAX_TURN_CARDS + 2 {
            markers.observe(&tool_result_message(id.clone(), marker, true));
        }
        assert_eq!(markers.cards.len(), MAX_TURN_CARDS);
    }

    #[tokio::test]
    async fn assistant_cards_are_delivered_as_ledgered_card_verbs() {
        let platform = Arc::new(RecordingPlatform::default());
        let dyn_platform: Arc<dyn GatewayPlatform> = platform.clone();
        let ledger = DeliveryLedger::in_memory().expect("in-memory ledger");
        let event = test_event();
        let cards = vec![
            serde_json::json!({"type": "AdaptiveCard", "body": []}),
            serde_json::json!({"blocks": [{"type": "section"}]}),
        ];

        deliver_assistant_cards(&dyn_platform, Some(&ledger), &event, "chat-1", &cards).await;
        // A redelivery of the same event must not enqueue or send a second copy.
        deliver_assistant_cards(&dyn_platform, Some(&ledger), &event, "chat-1", &cards).await;

        let verbs = platform.verbs.lock().unwrap();
        assert_eq!(verbs.len(), 2, "each card becomes exactly one card verb");
        for verb in verbs.iter() {
            let OutboundVerb::Card { payload } = verb else {
                panic!("expected a card verb, got {verb:?}");
            };
            assert!(payload.is_object());
        }
        assert_eq!(ledger.len().expect("ledger len"), 2);
    }

    #[tokio::test]
    async fn turns_without_cards_send_no_card_verbs() {
        let platform = Arc::new(RecordingPlatform::default());
        let dyn_platform: Arc<dyn GatewayPlatform> = platform.clone();
        let event = test_event();
        deliver_assistant_cards(&dyn_platform, None, &event, "chat-1", &[]).await;
        assert!(platform.verbs.lock().unwrap().is_empty());
    }

    /// A platform whose adapter refuses cards the way the trait default does.
    struct NoCardPlatform {
        verbs: Mutex<Vec<OutboundVerb>>,
    }

    #[async_trait::async_trait]
    impl GatewayPlatform for NoCardPlatform {
        fn name(&self) -> &str {
            "nocard"
        }

        async fn next_events(&self) -> Result<Vec<MessageEvent>, String> {
            Ok(Vec::new())
        }

        async fn send(
            &self,
            _chat_id: &str,
            _content: &str,
        ) -> vikram_gateway::platform::SendResult {
            vikram_gateway::platform::SendResult::ok("text-id")
        }

        async fn send_verb(
            &self,
            _chat_id: &str,
            verb: &OutboundVerb,
            _reply_to: Option<&str>,
        ) -> vikram_gateway::platform::SendResult {
            self.verbs.lock().unwrap().push(verb.clone());
            vikram_gateway::platform::SendResult::failed(
                "this platform does not send rich cards",
                Some("bad_format"),
            )
        }
    }

    #[tokio::test]
    async fn an_unsupported_platform_refuses_the_card_honestly() {
        let platform = Arc::new(NoCardPlatform {
            verbs: Mutex::new(Vec::new()),
        });
        let dyn_platform: Arc<dyn GatewayPlatform> = platform.clone();
        let ledger = DeliveryLedger::in_memory().expect("in-memory ledger");
        let event = test_event();
        let cards = vec![serde_json::json!({"type": "AdaptiveCard"})];

        // The refusal surfaces as a failed SendResult — logged, ledgered, never retried into
        // a fake success and never downgraded to text.
        deliver_assistant_cards(&dyn_platform, Some(&ledger), &event, "chat-1", &cards).await;

        assert_eq!(platform.verbs.lock().unwrap().len(), 1);
        assert_eq!(ledger.len().expect("ledger len"), 1);
    }

    #[test]
    fn a_draft_marker_line_parses_to_its_text() {
        let content = "composed a reply\nDRAFT:{\"text\":\"here is a preview\"}\ndone";
        assert_eq!(draft_markers(content), vec!["here is a preview".to_owned()]);
    }

    #[test]
    fn malformed_or_oversized_draft_markers_are_skipped() {
        for content in [
            "no marker here",
            "DRAFT:{not json}",
            "DRAFT:[1,2,3]",
            "DRAFT:\"just a string\"",
            "DRAFT:42",
            "DRAFT:null",
            "DRAFT:{}",
            "DRAFT:{\"text\":\"\"}",
            "DRAFT:{\"text\":\"   \"}",
            "DRAFT:{\"text\":42}",
            "DRAFT:{\"text\":\"hi\"} trailing",
        ] {
            assert!(draft_markers(content).is_empty(), "{content}");
        }
        // A payload over the byte cap is skipped even though it is valid JSON.
        let huge = format!(
            "DRAFT:{{\"text\":\"{}\"}}",
            "x".repeat(MAX_DRAFT_MARKER_BYTES)
        );
        assert!(draft_markers(&huge).is_empty());
        // A marker embedded mid-line is not a marker.
        assert!(draft_markers("prefix DRAFT:{\"text\":\"hi\"}").is_empty());
    }

    #[test]
    fn only_allowlisted_tools_raise_drafts_from_their_results() {
        let mut markers = TurnMarkers::default();
        let marker = "DRAFT:{\"text\":\"preview me\"}";

        // An MCP tool's marker is honored once its call is known.
        let (call_msg, mcp_id) = tool_call_message("mcp.drafts.create");
        markers.observe(&call_msg);
        markers.observe(&tool_result_message(mcp_id, marker, true));
        assert_eq!(markers.drafts.len(), 1);
        assert_eq!(markers.drafts[0], "preview me");

        // Builtin tools whose output is fetched or computed content never qualify — and
        // neither do the sibling chat markers: each marker family is gated independently.
        for name in [
            "web.fetch",
            "shell.run",
            "fs.read",
            "plugin.list",
            "message.poll",
            "message.card",
        ] {
            let (call_msg, id) = tool_call_message(name);
            markers.observe(&call_msg);
            markers.observe(&tool_result_message(id, marker, true));
        }
        // `plugin.execute` output is operator-installed plugin code, so it does qualify.
        let (call_msg, plugin_id) = tool_call_message("plugin.execute");
        markers.observe(&call_msg);
        markers.observe(&tool_result_message(plugin_id, marker, true));
        assert_eq!(markers.drafts.len(), 2);

        // `message.draft` is the one builtin whose whole purpose is emitting the marker.
        let (call_msg, draft_id) = tool_call_message("message.draft");
        markers.observe(&call_msg);
        markers.observe(&tool_result_message(draft_id, marker, true));
        assert_eq!(markers.drafts.len(), 3);

        // A failed result and an unknown call id raise nothing.
        let (call_msg, failed_id) = tool_call_message("mcp.drafts.create");
        markers.observe(&call_msg);
        markers.observe(&tool_result_message(failed_id, marker, false));
        markers.observe(&tool_result_message(ToolCallId::generate(), marker, true));
        assert_eq!(markers.drafts.len(), 3);
    }

    #[test]
    fn a_turn_carries_at_most_the_draft_cap() {
        let mut markers = TurnMarkers::default();
        let (call_msg, id) = tool_call_message("mcp.drafts.create");
        markers.observe(&call_msg);
        let marker = "DRAFT:{\"text\":\"preview\"}";
        for _ in 0..MAX_TURN_DRAFTS + 2 {
            markers.observe(&tool_result_message(id.clone(), marker, true));
        }
        assert_eq!(markers.drafts.len(), MAX_TURN_DRAFTS);
    }

    #[test]
    fn draft_ids_are_stable_per_delivery_and_distinct_per_marker() {
        let first = draft_id_for("evt-1:draft:0");
        assert_eq!(first, draft_id_for("evt-1:draft:0"));
        assert_ne!(first, draft_id_for("evt-1:draft:1"));
        assert_ne!(first, draft_id_for("evt-2:draft:0"));
    }

    #[tokio::test]
    async fn assistant_drafts_are_delivered_as_ledgered_draft_verbs() {
        let platform = Arc::new(RecordingPlatform::default());
        let dyn_platform: Arc<dyn GatewayPlatform> = platform.clone();
        let ledger = DeliveryLedger::in_memory().expect("in-memory ledger");
        let event = test_event();
        let drafts = vec!["first preview".to_owned(), "second preview".to_owned()];

        deliver_assistant_drafts(&dyn_platform, Some(&ledger), &event, "chat-1", &drafts).await;
        // A redelivery of the same event must not enqueue or send a second copy.
        deliver_assistant_drafts(&dyn_platform, Some(&ledger), &event, "chat-1", &drafts).await;

        let verbs = platform.verbs.lock().unwrap();
        assert_eq!(verbs.len(), 2, "each draft becomes exactly one draft verb");
        let mut draft_ids = Vec::new();
        for (index, verb) in verbs.iter().enumerate() {
            let OutboundVerb::Draft { draft_id, content } = verb else {
                panic!("expected a draft verb, got {verb:?}");
            };
            assert_eq!(content, &drafts[index]);
            draft_ids.push(*draft_id);
        }
        assert_ne!(draft_ids[0], draft_ids[1]);
        assert_eq!(ledger.len().expect("ledger len"), 2);
    }

    #[tokio::test]
    async fn turns_without_drafts_send_no_draft_verbs() {
        let platform = Arc::new(RecordingPlatform::default());
        let dyn_platform: Arc<dyn GatewayPlatform> = platform.clone();
        let event = test_event();
        deliver_assistant_drafts(&dyn_platform, None, &event, "chat-1", &[]).await;
        assert!(platform.verbs.lock().unwrap().is_empty());
    }

    /// A platform whose adapter refuses drafts the way the trait default does.
    struct NoDraftPlatform {
        verbs: Mutex<Vec<OutboundVerb>>,
    }

    #[async_trait::async_trait]
    impl GatewayPlatform for NoDraftPlatform {
        fn name(&self) -> &str {
            "nodraft"
        }

        async fn next_events(&self) -> Result<Vec<MessageEvent>, String> {
            Ok(Vec::new())
        }

        async fn send(
            &self,
            _chat_id: &str,
            _content: &str,
        ) -> vikram_gateway::platform::SendResult {
            vikram_gateway::platform::SendResult::ok("text-id")
        }

        async fn send_verb(
            &self,
            _chat_id: &str,
            verb: &OutboundVerb,
            _reply_to: Option<&str>,
        ) -> vikram_gateway::platform::SendResult {
            self.verbs.lock().unwrap().push(verb.clone());
            vikram_gateway::platform::SendResult::failed(
                "this platform does not stream drafts",
                Some("bad_format"),
            )
        }
    }

    #[tokio::test]
    async fn an_unsupported_platform_refuses_the_draft_honestly() {
        let platform = Arc::new(NoDraftPlatform {
            verbs: Mutex::new(Vec::new()),
        });
        let dyn_platform: Arc<dyn GatewayPlatform> = platform.clone();
        let ledger = DeliveryLedger::in_memory().expect("in-memory ledger");
        let event = test_event();
        let drafts = vec!["preview".to_owned()];

        // The refusal surfaces as a failed SendResult — logged, ledgered, never retried into
        // a fake success and never downgraded to text.
        deliver_assistant_drafts(&dyn_platform, Some(&ledger), &event, "chat-1", &drafts).await;

        assert_eq!(platform.verbs.lock().unwrap().len(), 1);
        assert_eq!(ledger.len().expect("ledger len"), 1);
    }

    #[test]
    fn a_reaction_marker_line_parses_to_emoji_and_remove() {
        let content =
            "reacted\nREACT:{\"emoji\":\"👍\",\"remove\":false}\nREACT:{\"emoji\":\"🎉\"}\ndone";
        assert_eq!(
            reaction_markers(content),
            vec![
                ("👍".to_owned(), false),
                // `remove` defaults to false when the marker omits it.
                ("🎉".to_owned(), false),
            ]
        );
        let content = "REACT:{\"emoji\":\"👀\",\"remove\":true}";
        assert_eq!(reaction_markers(content), vec![("👀".to_owned(), true)]);
    }

    #[test]
    fn malformed_or_untargetable_reaction_markers_are_skipped() {
        for content in [
            "no marker here",
            "REACT:{not json}",
            "REACT:[1,2,3]",
            "REACT:{\"emoji\":\"\"}",
            "REACT:{\"emoji\":\"   \"}",
            "REACT:{\"emoji\":42}",
            "REACT:{\"emoji\":\"hi\"} trailing",
            "prefix REACT:{\"emoji\":\"👍\"}",
        ] {
            assert!(reaction_markers(content).is_empty(), "{content}");
        }
        // An emoji over the byte cap is skipped even though it is valid JSON.
        let huge = format!(
            "REACT:{{\"emoji\":\"{}\"}}",
            "x".repeat(MAX_REACTION_EMOJI_BYTES + 1)
        );
        assert!(reaction_markers(&huge).is_empty());
    }

    #[test]
    fn only_allowlisted_tools_raise_reactions_from_their_results() {
        let mut markers = TurnMarkers::default();
        let marker = "REACT:{\"emoji\":\"👍\"}";

        // An MCP tool's marker is honored once its call is known.
        let (call_msg, mcp_id) = tool_call_message("mcp.reactions.add");
        markers.observe(&call_msg);
        markers.observe(&tool_result_message(mcp_id, marker, true));
        assert_eq!(markers.reactions.len(), 1);

        // Builtin tools whose output is fetched or computed content never qualify — and
        // neither do the sibling chat markers: each marker family is gated independently.
        for name in [
            "web.fetch",
            "shell.run",
            "fs.read",
            "plugin.list",
            "message.poll",
            "message.card",
            "message.draft",
        ] {
            let (call_msg, id) = tool_call_message(name);
            markers.observe(&call_msg);
            markers.observe(&tool_result_message(id, marker, true));
        }
        // `plugin.execute` output is operator-installed plugin code, so it does qualify.
        let (call_msg, plugin_id) = tool_call_message("plugin.execute");
        markers.observe(&call_msg);
        markers.observe(&tool_result_message(plugin_id, marker, true));
        assert_eq!(markers.reactions.len(), 2);

        // `message.react` is the one builtin whose whole purpose is emitting the marker.
        let (call_msg, react_id) = tool_call_message("message.react");
        markers.observe(&call_msg);
        markers.observe(&tool_result_message(react_id, marker, true));
        assert_eq!(markers.reactions.len(), 3);

        // A failed result and an unknown call id raise nothing.
        let (call_msg, failed_id) = tool_call_message("mcp.reactions.add");
        markers.observe(&call_msg);
        markers.observe(&tool_result_message(failed_id, marker, false));
        markers.observe(&tool_result_message(ToolCallId::generate(), marker, true));
        assert_eq!(markers.reactions.len(), 3);
    }

    #[test]
    fn a_turn_carries_at_most_the_reaction_cap() {
        let mut markers = TurnMarkers::default();
        let (call_msg, id) = tool_call_message("mcp.reactions.add");
        markers.observe(&call_msg);
        let marker = "REACT:{\"emoji\":\"👍\"}";
        for _ in 0..MAX_TURN_REACTIONS + 2 {
            markers.observe(&tool_result_message(id.clone(), marker, true));
        }
        assert_eq!(markers.reactions.len(), MAX_TURN_REACTIONS);
    }

    #[tokio::test]
    async fn assistant_reactions_target_the_triggering_message() {
        let platform = Arc::new(RecordingPlatform::default());
        let dyn_platform: Arc<dyn GatewayPlatform> = platform.clone();
        let ledger = DeliveryLedger::in_memory().expect("in-memory ledger");
        let event = test_event();
        let reactions = vec![("👍".to_owned(), false), ("👀".to_owned(), true)];

        deliver_assistant_reactions(&dyn_platform, Some(&ledger), &event, "chat-1", &reactions)
            .await;
        // A redelivery of the same event must not enqueue or send a second copy.
        deliver_assistant_reactions(&dyn_platform, Some(&ledger), &event, "chat-1", &reactions)
            .await;

        let verbs = platform.verbs.lock().unwrap();
        assert_eq!(verbs.len(), 2, "each reaction becomes exactly one verb");
        let OutboundVerb::Reaction {
            message_id,
            emoji,
            remove,
        } = &verbs[0]
        else {
            panic!("expected a reaction verb, got {:?}", verbs[0]);
        };
        assert_eq!(message_id, "m-1", "the target is the inbound message id");
        assert_eq!(emoji, "👍");
        assert!(!remove);
        let OutboundVerb::Reaction { remove, .. } = &verbs[1] else {
            panic!("expected a reaction verb, got {:?}", verbs[1]);
        };
        assert!(remove);
        assert_eq!(ledger.len().expect("ledger len"), 2);
    }

    #[tokio::test]
    async fn reactions_fall_back_to_the_source_message_id() {
        let platform = Arc::new(RecordingPlatform::default());
        let dyn_platform: Arc<dyn GatewayPlatform> = platform.clone();
        let mut event = test_event();
        event.message_id = None;
        event.source.message_id = Some("src-m-9".to_owned());
        let reactions = vec![("🎉".to_owned(), false)];

        deliver_assistant_reactions(&dyn_platform, None, &event, "chat-1", &reactions).await;

        let verbs = platform.verbs.lock().unwrap();
        assert_eq!(verbs.len(), 1);
        let OutboundVerb::Reaction { message_id, .. } = &verbs[0] else {
            panic!("expected a reaction verb, got {:?}", verbs[0]);
        };
        assert_eq!(message_id, "src-m-9");
    }

    #[tokio::test]
    async fn an_event_without_a_message_id_cannot_be_reacted_to() {
        let platform = Arc::new(RecordingPlatform::default());
        let dyn_platform: Arc<dyn GatewayPlatform> = platform.clone();
        let mut event = test_event();
        event.message_id = None;
        let reactions = vec![("👍".to_owned(), false)];

        deliver_assistant_reactions(&dyn_platform, None, &event, "chat-1", &reactions).await;

        assert!(platform.verbs.lock().unwrap().is_empty());
    }

    #[tokio::test]
    async fn turns_without_reactions_send_no_reaction_verbs() {
        let platform = Arc::new(RecordingPlatform::default());
        let dyn_platform: Arc<dyn GatewayPlatform> = platform.clone();
        let event = test_event();
        deliver_assistant_reactions(&dyn_platform, None, &event, "chat-1", &[]).await;
        assert!(platform.verbs.lock().unwrap().is_empty());
    }
}
