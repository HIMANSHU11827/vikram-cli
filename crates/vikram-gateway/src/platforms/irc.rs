//! IRC adapter — line protocol over a socket.
//!
//! Implements the client side of a plain IRC connection: `NICK`/`USER` registration, `JOIN`, text
//! `PRIVMSG`, `PING`/`PONG`, and inbound `PRIVMSG` parsing into gateway events. The socket is
//! injected through [`crate::socket::LineSocket`], so the whole adapter is testable without a
//! server.
//!
//! Not ported: TLS (`ircs://`), SASL authentication, NickServ identification, CTCP actions,
//! multi-channel membership, and reconnect/backoff (the host supervises the socket).

use async_trait::async_trait;
use std::sync::Arc;

use crate::event::{MessageEvent, Platform, SessionSource};
use crate::helpers::{codepoint_len, truncate_message};
use crate::platform::{ChatInfo, PlatformAdapter, SendResult};
use crate::socket::SharedLineSocket;

pub const IRC_MAX_MESSAGE_LENGTH: usize = 450;

pub struct IrcAdapter {
    socket: SharedLineSocket,
    nickname: String,
    channel: String,
}

impl IrcAdapter {
    pub fn new(
        socket: SharedLineSocket,
        nickname: impl Into<String>,
        channel: impl Into<String>,
    ) -> Self {
        Self {
            socket: Arc::clone(&socket),
            nickname: nickname.into(),
            channel: channel.into(),
        }
    }

    /// The wire name we register with. Also how inbound lines are matched to us.
    pub fn nickname(&self) -> &str {
        &self.nickname
    }

    /// Send one raw line (used by tests and by the host for housekeeping).
    pub async fn send_raw(&self, line: &str) -> bool {
        self.socket.send_line(line).await.is_ok()
    }

    /// Read one line and normalize it. Returns `Ok(None)` for lines that carry no message (server
    /// numerics, our own joins) and `Err` only when the stream fails.
    pub async fn next_event(&self) -> Result<Option<MessageEvent>, String> {
        loop {
            let Some(line) = self
                .socket
                .next_line()
                .await
                .map_err(|err| err.to_string())?
            else {
                return Ok(None);
            };
            // Keep the connection alive while we are only reading.
            if let Some(token) = line.strip_prefix("PING ") {
                let _ = self.socket.send_line(&format!("PONG {token}")).await;
                continue;
            }
            if let Some(event) = parse_privmsg(&line, &self.nickname) {
                return Ok(Some(event));
            }
        }
    }
}

/// Parse `:nick!user@host PRIVMSG #chan :text` into an event, skipping our own echo.
fn parse_privmsg(line: &str, own_nick: &str) -> Option<MessageEvent> {
    let rest = line.strip_prefix(':')?;
    let mut parts = rest.splitn(3, ' ');
    let prefix = parts.next()?;
    let command = parts.next()?;
    if !command.eq_ignore_ascii_case("PRIVMSG") {
        return None;
    }
    let tail = parts.next()?;
    let (target, text) = tail.split_once(" :")?;
    let nick = prefix.split('!').next().unwrap_or(prefix);
    if nick.eq_ignore_ascii_case(own_nick) {
        return None;
    }
    let text = text.trim();
    if text.is_empty() {
        return None;
    }

    let chat_type = if target.starts_with('#') || target.starts_with('&') {
        "group"
    } else {
        "dm"
    };
    let platform = Platform::new("irc").expect("valid platform");
    let mut source = SessionSource::new(platform, target);
    source.chat_type = chat_type.to_string();
    source.user_id = Some(nick.to_string());
    source.user_name = Some(nick.to_string());

    let mut event = MessageEvent::new(text.to_string(), source);
    event.user_id = Some(nick.to_string());
    event.user_name = Some(nick.to_string());
    event.raw_message = Some(serde_json::json!({ "line": line }));
    Some(event)
}

#[async_trait]
impl PlatformAdapter for IrcAdapter {
    async fn connect(&mut self, _is_reconnect: bool) -> bool {
        if self
            .socket
            .send_line(&format!("NICK {}", self.nickname))
            .await
            .is_err()
        {
            return false;
        }
        if self
            .socket
            .send_line(&format!("USER {} 0 * :{}", self.nickname, self.nickname))
            .await
            .is_err()
        {
            return false;
        }
        if !self.channel.is_empty()
            && self
                .socket
                .send_line(&format!("JOIN {}", self.channel))
                .await
                .is_err()
        {
            return false;
        }
        true
    }

    async fn disconnect(&mut self) {
        let _ = self.socket.send_line("QUIT :vikram gateway").await;
    }

    async fn send(
        &self,
        chat_id: &str,
        content: &str,
        _reply_to: Option<&str>,
        _metadata: Option<serde_json::Value>,
    ) -> SendResult {
        let target = if chat_id.trim().is_empty() {
            self.channel.trim()
        } else {
            chat_id.trim()
        };
        if target.is_empty() {
            return SendResult::failed("irc needs a channel or nick", Some("bad_format"));
        }
        if content.trim().is_empty() {
            return SendResult::failed("empty message content", Some("bad_format"));
        }
        let chunks = truncate_message(content, IRC_MAX_MESSAGE_LENGTH, codepoint_len);
        for chunk in &chunks {
            // IRC has no message ids and no acknowledgement: a refused line is the only failure.
            if let Err(err) = self
                .socket
                .send_line(&format!("PRIVMSG {target} :{chunk}"))
                .await
            {
                return SendResult::failed(format!("irc send failed: {err}"), Some("transient"))
                    .retryable(None);
            }
        }
        SendResult::ok("")
    }

    fn splits_long_messages(&self) -> bool {
        true
    }

    fn max_message_length(&self) -> usize {
        IRC_MAX_MESSAGE_LENGTH
    }

    async fn get_chat_info(&self, chat_id: &str) -> ChatInfo {
        ChatInfo {
            name: chat_id.to_string(),
            chat_type: if chat_id.starts_with('#') {
                "group".to_string()
            } else {
                "dm".to_string()
            },
        }
    }

    async fn send_typing(&self, _chat_id: &str, _metadata: Option<serde_json::Value>) {}
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::socket::{LineSocket, SharedLineSocket, SocketError};
    use async_trait::async_trait;
    use std::sync::Mutex;

    struct MockSocket {
        sent: Mutex<Vec<String>>,
        incoming: Mutex<std::collections::VecDeque<String>>,
    }

    impl MockSocket {
        fn new(incoming: Vec<String>) -> Arc<Self> {
            Arc::new(Self {
                sent: Mutex::new(Vec::new()),
                incoming: Mutex::new(incoming.into()),
            })
        }
        fn sent(&self) -> Vec<String> {
            self.sent.lock().unwrap().clone()
        }
    }

    #[async_trait]
    impl LineSocket for MockSocket {
        async fn send_line(&self, line: &str) -> Result<(), SocketError> {
            self.sent.lock().unwrap().push(line.to_string());
            Ok(())
        }
        async fn next_line(&self) -> Result<Option<String>, SocketError> {
            Ok(self.incoming.lock().unwrap().pop_front())
        }
    }

    fn build(incoming: Vec<String>) -> (IrcAdapter, Arc<MockSocket>) {
        let socket = MockSocket::new(incoming);
        let adapter = IrcAdapter::new(
            Arc::clone(&socket) as SharedLineSocket,
            "vikram",
            "#general",
        );
        (adapter, socket)
    }

    #[tokio::test]
    async fn connect_registers_and_joins() {
        let (mut adapter, socket) = build(vec![]);
        assert!(adapter.connect(false).await);
        assert_eq!(
            socket.sent(),
            vec![
                "NICK vikram".to_string(),
                "USER vikram 0 * :vikram".to_string(),
                "JOIN #general".to_string()
            ]
        );
    }

    #[tokio::test]
    async fn send_posts_privmsg_and_chunks() {
        let (adapter, socket) = build(vec![]);
        assert!(adapter.send("#general", "hello", None, None).await.success);
        assert_eq!(socket.sent(), vec!["PRIVMSG #general :hello"]);

        let long = "x".repeat(IRC_MAX_MESSAGE_LENGTH + 1);
        assert!(adapter.send("#general", &long, None, None).await.success);
        let sent = socket.sent();
        assert_eq!(sent.len(), 3);
        for line in &sent[1..] {
            let body = line.split_once(" :").expect("body").1;
            assert!(body.chars().count() <= IRC_MAX_MESSAGE_LENGTH);
        }
    }

    #[tokio::test]
    async fn a_blank_target_falls_back_to_the_configured_channel() {
        let (adapter, socket) = build(vec![]);
        assert!(adapter.send("", "hi", None, None).await.success);
        assert_eq!(socket.sent(), vec!["PRIVMSG #general :hi"]);
    }

    #[tokio::test]
    async fn blank_content_does_not_touch_the_socket() {
        let (adapter, socket) = build(vec![]);
        assert!(!adapter.send("#c", "  ", None, None).await.success);
        assert!(socket.sent().is_empty());
    }

    #[tokio::test]
    async fn inbound_privmsg_becomes_an_event_and_ping_is_answered() {
        let (adapter, socket) = build(vec![
            "PING :tok".to_string(),
            "PONG :tok".to_string(),
            ":alice!u@h PRIVMSG #general :hello there".to_string(),
        ]);
        let event = adapter.next_event().await.unwrap().expect("event");
        assert_eq!(event.text, "hello there");
        assert_eq!(event.source.chat_id, "#general");
        assert_eq!(event.source.chat_type, "group");
        assert_eq!(event.user_id.as_deref(), Some("alice"));
        assert!(socket.sent().contains(&"PONG :tok".to_string()));
    }

    #[tokio::test]
    async fn our_own_echo_is_skipped() {
        let (adapter, _) = build(vec![
            ":vikram!u@h PRIVMSG #general :my own words".to_string(),
            ":bob!u@h PRIVMSG #general :hi".to_string(),
        ]);
        let event = adapter.next_event().await.unwrap().expect("event");
        assert_eq!(event.user_id.as_deref(), Some("bob"));
    }

    #[tokio::test]
    async fn end_of_stream_is_none() {
        let (adapter, _) = build(vec![]);
        assert!(adapter.next_event().await.unwrap().is_none());
    }

    #[tokio::test]
    async fn chat_info_and_typing_are_local() {
        let (adapter, socket) = build(vec![]);
        assert_eq!(adapter.get_chat_info("#general").await.chat_type, "group");
        assert_eq!(adapter.get_chat_info("bob").await.chat_type, "dm");
        adapter.send_typing("#general", None).await;
        assert!(socket.sent().is_empty());
    }
}
