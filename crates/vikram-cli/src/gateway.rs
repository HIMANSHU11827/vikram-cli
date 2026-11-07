//! `vikram gateway` — run configured chat platforms against the runtime.
//!
//! Each configured platform gets one host loop: read events, map the chat to a session, submit a
//! turn, then send the turn's text back. Configuration is environment-only for now, matching the
//! variable names in `docs/gateway-platforms.md`; a platform with no credentials is skipped with a
//! line explaining what is missing.
//!
//! The gateway is a headless surface, so it runs unattended: anything needing approval is refused
//! rather than waiting for a human who is not there.

use std::collections::HashMap;
use std::sync::{Arc, Mutex};

use vikram_contracts::{ImageSource, Message, PermissionMode};
use vikram_gateway::event::MessageEvent;
use vikram_gateway::host::{
    BuzzPlatform, ChannelPlatform, DingTalkStreamPlatform, DiscordPlatform, EmailPlatform,
    GatewayPlatform, IrcPlatform, MatrixPlatform, MattermostPlatform, NtfyPlatform, PhotonPlatform,
    SignalPlatform, SimplexPlatform, TelegramPlatform, WeComPlatform, WeixinPlatform,
    WhatsAppPlatform, YuanbaoPlatform, serve,
};
use vikram_gateway::ingress::{
    WebhookHandler, WebhookHub, WebhookPlatform, event_route, serve as serve_webhooks,
};
use vikram_gateway::media_fetch::{MediaFetchLimits, fetch_images_with_skips};
use vikram_gateway::platform::MessageHandler;
use vikram_gateway::platform::PlatformAdapter;
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
use vikram_gateway::socket::{TcpLineSocket, TlsLineSocket, WsBinarySocket, WsSocket};
use vikram_gateway::transport::{ReqwestTransport, SharedTransport};
use vikram_runtime::config::Config;
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

/// A gateway process reads one variable per credential; empty means "not configured".
fn env(name: &str) -> Option<String> {
    std::env::var(name)
        .ok()
        .map(|value| value.trim().to_string())
        .filter(|value| !value.is_empty())
}

pub async fn run(config: Config) -> Result<(), String> {
    let runtime = Runtime::start(config)
        .await
        .map_err(|err| format!("could not start runtime: {err}"))?;
    let handle = runtime.handle();

    let mut platforms: Vec<Arc<dyn GatewayPlatform>> = Vec::new();
    let mut skipped: Vec<String> = Vec::new();

    if let Some(token) = env("TELEGRAM_BOT_TOKEN") {
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

    if let Some(token) = env("DISCORD_BOT_TOKEN") {
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

    match (env("SIGNAL_HTTP_URL"), env("SIGNAL_ACCOUNT")) {
        (Some(base), Some(account)) => {
            let transport = ReqwestTransport::new().map_err(|err| err.to_string())?;
            platforms.push(Arc::new(SignalPlatform::new(SignalAdapter::new(
                transport.into_shared(),
                base,
                account,
            ))));
        }
        _ => skipped.push("signal (SIGNAL_HTTP_URL, SIGNAL_ACCOUNT)".into()),
    }

    match env("NTFY_TOPIC") {
        Some(topic) => {
            let transport = ReqwestTransport::new().map_err(|err| err.to_string())?;
            let server = env("NTFY_SERVER_URL").unwrap_or_else(|| "https://ntfy.sh".into());
            platforms.push(Arc::new(NtfyPlatform::new(NtfyAdapter::new(
                transport.into_shared(),
                server,
                topic,
                env("NTFY_TOKEN"),
            ))));
        }
        None => skipped.push("ntfy (NTFY_TOPIC)".into()),
    }

    match (env("WEIXIN_ACCOUNT_ID"), env("WEIXIN_TOKEN")) {
        (Some(account), Some(token)) => {
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

    if let Some(url) = env("SIMPLEX_WS_URL") {
        let socket = WsSocket::connect(&url).map_err(|err| err.to_string())?;
        platforms.push(Arc::new(SimplexPlatform::new(SimplexAdapter::new(
            socket.into_shared(),
        ))));
    } else {
        skipped.push("simplex (SIMPLEX_WS_URL)".into());
    }

    match (env("IRC_SERVER"), env("IRC_NICKNAME")) {
        (Some(server), Some(nickname)) => {
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

    match (env("MATRIX_HOMESERVER"), env("MATRIX_ACCESS_TOKEN")) {
        (Some(homeserver), Some(token)) => {
            let transport = ReqwestTransport::new().map_err(|err| err.to_string())?;
            let adapter = MatrixAdapter::new(transport.into_shared(), homeserver, token);
            platforms.push(Arc::new(MatrixPlatform::new(adapter)));
        }
        _ => skipped.push("matrix (MATRIX_HOMESERVER, MATRIX_ACCESS_TOKEN)".into()),
    }

    match (
        env("MATTERMOST_URL"),
        env("MATTERMOST_TOKEN"),
        env("MATTERMOST_CHANNEL_ID"),
    ) {
        (Some(base), Some(token), Some(channel)) => {
            let transport = ReqwestTransport::new().map_err(|err| err.to_string())?;
            let adapter = MattermostAdapter::new(transport.into_shared(), base, token);
            platforms.push(Arc::new(MattermostPlatform::new(adapter, channel)));
        }
        _ => skipped
            .push("mattermost (MATTERMOST_URL, MATTERMOST_TOKEN, MATTERMOST_CHANNEL_ID)".into()),
    }

    if let Some(base) = env("WHATSAPP_BRIDGE_URL") {
        let transport = ReqwestTransport::new().map_err(|err| err.to_string())?;
        platforms.push(Arc::new(WhatsAppPlatform::new(WhatsAppAdapter::new(
            transport.into_shared(),
            base,
        ))));
    } else {
        skipped.push("whatsapp (WHATSAPP_BRIDGE_URL)".into());
    }

    match (env("WECOM_BOT_ID"), env("WECOM_SECRET")) {
        (Some(bot_id), Some(secret)) => {
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
        env("EMAIL_FROM"),
        env("EMAIL_SMTP_HOST"),
        env("EMAIL_IMAP_HOST"),
    ) {
        (Some(from), Some(smtp_host), Some(imap_host)) => {
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

    match (env("YUANBAO_APP_ID"), env("YUANBAO_APP_SECRET")) {
        (Some(app_id), Some(app_secret)) => {
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

    match (env("BUZZ_RELAY_URL"), env("BUZZ_PRIVATE_KEY")) {
        (Some(_), Some(_)) => match BuzzKeypair::from_env() {
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

    match (env("DINGTALK_CLIENT_ID"), env("DINGTALK_CLIENT_SECRET")) {
        (Some(client_id), Some(client_secret)) => {
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

    match (env("PHOTON_SIDECAR_URL"), env("PHOTON_SIDECAR_TOKEN")) {
        (Some(url), Some(token)) => {
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

    match (env("BLUEBUBBLES_SERVER_URL"), env("BLUEBUBBLES_PASSWORD")) {
        (Some(server), Some(password)) => {
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
        env("WHATSAPP_CLOUD_ACCESS_TOKEN"),
        env("WHATSAPP_CLOUD_PHONE_NUMBER_ID"),
        env("WHATSAPP_CLOUD_APP_SECRET"),
        env("WHATSAPP_CLOUD_VERIFY_TOKEN"),
    ) {
        (Some(token), Some(phone_id), Some(app_secret), Some(verify_token)) => {
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
        env("TWILIO_ACCOUNT_SID"),
        env("TWILIO_AUTH_TOKEN"),
        env("TWILIO_FROM_NUMBER"),
        env("TWILIO_WEBHOOK_URL"),
    ) {
        (Some(account_sid), Some(auth_token), Some(from), Some(webhook_url)) => {
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
        if let (Some(app_id), Some(app_secret)) = (env("FEISHU_APP_ID"), env("FEISHU_APP_SECRET")) {
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

        match (env("LINE_CHANNEL_ACCESS_TOKEN"), env("LINE_CHANNEL_SECRET")) {
            (Some(token), Some(secret)) => {
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

        match (env("SLACK_BOT_TOKEN"), env("SLACK_SIGNING_SECRET")) {
            (Some(bot_token), Some(secret)) => {
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
        match (env("QQ_BOT_APP_ID"), env("QQ_BOT_SECRET")) {
            (Some(app_id), Some(secret)) => {
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
        match env("RAFT_BRIDGE_TOKEN") {
            Some(token) => {
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
            None => skipped.push(
                "raft (RAFT_BRIDGE_TOKEN; optional RAFT_RUNTIME_SESSION, RAFT_PATH, RAFT_HOST, RAFT_PORT)"
                    .into(),
            ),
        }

        // Google Chat signs its webhook POSTs with a Google JWT.
        match (
            env("GOOGLE_CHAT_WEBHOOK_URL"),
            env("GOOGLE_CHAT_PROJECT_NUMBER"),
        ) {
            (Some(webhook_url), Some(project)) => {
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
            env("TEAMS_TENANT_ID"),
            env("TEAMS_CLIENT_ID"),
            env("TEAMS_CLIENT_SECRET"),
        ) {
            (Some(tenant), Some(client_id), Some(client_secret)) => {
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

        // A2A needs no credentials: it is the agent-facing protocol itself.
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
                Box::pin(async move {
                    if request.method != "POST" {
                        return (405, "method not allowed".to_string());
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

    if platforms.is_empty() {
        return Err(format!(
            "no gateway platform is configured\nmissing: {}",
            skipped.join(", ")
        ));
    }
    for line in &skipped {
        eprintln!("[gateway] not configured: {line}");
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
    let sessions: Arc<Mutex<HashMap<String, vikram_contracts::SessionId>>> =
        Arc::new(Mutex::new(HashMap::new()));

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
    for platform in platforms {
        let handle = handle.clone();
        let sessions = Arc::clone(&sessions);
        let media = Arc::clone(&media_transport);
        let handler: vikram_gateway::host::ReplyHandler = Arc::new(move |event| {
            turn(handle.clone(), Arc::clone(&sessions), media.clone(), event)
        });
        let stop = stop_rx.clone();
        let name = platform.name().to_string();
        let platform_for_log = Arc::clone(&platform);
        tasks.push(tokio::spawn(async move {
            let stats = serve(platform, handler, stop).await;
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

/// One inbound event → one runtime turn → the turn's text.
fn turn(
    handle: RuntimeHandle,
    sessions: Arc<Mutex<HashMap<String, vikram_contracts::SessionId>>>,
    media_transport: SharedTransport,
    event: MessageEvent,
) -> vikram_gateway::host::ReplyFuture {
    Box::pin(async move {
        let chat = event.source.chat_id.clone();
        let existing = sessions.lock().ok().and_then(|map| map.get(&chat).cloned());
        let session = match existing {
            Some(session) => session,
            None => {
                let title = format!("{} · {}", event.source.platform, chat);
                let session = handle.create_session(Some(title)).ok()?;
                if let Ok(mut map) = sessions.lock() {
                    map.insert(chat.clone(), session.clone());
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
        loop {
            match events.recv().await {
                Ok(RuntimeEvent::StreamDelta {
                    session: sid, text, ..
                }) if sid == session => reply.push_str(&text),
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
                Ok(_) => {}
                Err(_) => break,
            }
        }
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
    use vikram_contracts::Trust;

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
}
