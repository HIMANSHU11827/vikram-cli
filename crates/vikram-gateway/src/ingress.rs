//! HTTP ingress for webhook-shaped platforms.
//!
//! Slack, Feishu, LINE, WhatsApp Cloud, Teams and friends push events to a URL; they cannot be
//! polled. This module is the smallest server that can receive those pushes: HTTP/1.1, one request
//! per connection, `Content-Length` bodies, exact-path routing. It is deliberately not a framework
//! — the gateway only needs to turn a POST into bytes for an adapter's `parse_webhook`.
//!
//! Verification is the route's job, not the server's: a route handler calls the adapter's own
//! signature/token check before parsing, and returns 403 when it fails.

use std::collections::HashMap;
use std::future::Future;
use std::pin::Pin;
use std::sync::Arc;

use async_trait::async_trait;
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::TcpListener;
use tokio::sync::{Mutex, mpsc, watch};

use crate::event::MessageEvent;
use crate::host::{GatewayPlatform, ReplyFuture};
use crate::platform::{OutboundMedia, PlatformAdapter, SendResult};

/// Largest request head accepted (request line + headers). Bodies have their own cap.
pub const MAX_HEAD_BYTES: usize = 64 * 1024;
/// Largest webhook body accepted.
pub const MAX_BODY_BYTES: usize = 4 * 1024 * 1024;

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct WebhookRequest {
    pub method: String,
    pub path: String,
    pub headers: Vec<(String, String)>,
    pub body: Vec<u8>,
}

impl WebhookRequest {
    pub fn header(&self, name: &str) -> Option<&str> {
        self.headers
            .iter()
            .find(|(key, _)| key.eq_ignore_ascii_case(name))
            .map(|(_, value)| value.as_str())
    }

    pub fn body_text(&self) -> Result<&str, String> {
        std::str::from_utf8(&self.body).map_err(|err| format!("body is not utf-8: {err}"))
    }
}

/// `(method, path, headers, content_length)` from a request head.
pub type ParsedHead = (String, String, Vec<(String, String)>, usize);

/// Parse the head of an HTTP/1.1 request. `head` ends with a blank line; `body` is exactly
/// `Content-Length` bytes.
pub fn parse_head(head: &str) -> Option<ParsedHead> {
    let mut lines = head.split("\r\n");
    let request_line = lines.next()?;
    let mut parts = request_line.split_whitespace();
    let method = parts.next()?.to_string();
    let path = parts.next()?.to_string();
    let version = parts.next()?;
    if !version.starts_with("HTTP/1.") {
        return None;
    }
    let mut headers = Vec::new();
    for line in lines {
        if line.is_empty() {
            break;
        }
        let (name, value) = line.split_once(':')?;
        headers.push((name.trim().to_string(), value.trim().to_string()));
    }
    let length = headers
        .iter()
        .find(|(name, _)| name.eq_ignore_ascii_case("content-length"))
        .and_then(|(_, value)| value.parse::<usize>().ok())
        .unwrap_or(0);
    Some((method, path, headers, length))
}

#[async_trait]
pub trait WebhookHandler: Send + Sync {
    /// Return the status and a short body. Called once per request.
    async fn handle(&self, request: WebhookRequest) -> (u16, String);
}

pub struct WebhookHub {
    routes: HashMap<String, Arc<dyn WebhookHandler>>,
}

impl WebhookHub {
    pub fn new() -> Self {
        Self {
            routes: HashMap::new(),
        }
    }

    pub fn route(mut self, path: impl Into<String>, handler: Arc<dyn WebhookHandler>) -> Self {
        self.routes.insert(path.into(), handler);
        self
    }

    pub fn paths(&self) -> Vec<&str> {
        self.routes.keys().map(String::as_str).collect()
    }
}

impl Default for WebhookHub {
    fn default() -> Self {
        Self::new()
    }
}

#[async_trait]
impl WebhookHandler for WebhookHub {
    async fn handle(&self, request: WebhookRequest) -> (u16, String) {
        match self.routes.get(&request.path) {
            Some(route) => route.handle(request).await,
            None => (404, "not found".to_string()),
        }
    }
}

/// Serve the hub until `stop` is set. The bind string is `host:port`.
pub async fn serve(
    bind: &str,
    hub: Arc<dyn WebhookHandler>,
    mut stop: watch::Receiver<bool>,
) -> Result<(), String> {
    let listener = TcpListener::bind(bind)
        .await
        .map_err(|err| format!("cannot bind {bind}: {err}"))?;
    eprintln!("[gateway] webhook ingress listening on {bind}");
    loop {
        tokio::select! {
            changed = stop.changed() => {
                if changed.is_err() || *stop.borrow() {
                    return Ok(());
                }
            }
            accepted = listener.accept() => {
                let Ok((stream, _peer)) = accepted else { continue };
                let hub = Arc::clone(&hub);
                tokio::spawn(async move {
                    let _ = handle_connection(stream, hub).await;
                });
            }
        }
    }
}

async fn handle_connection(
    mut stream: tokio::net::TcpStream,
    hub: Arc<dyn WebhookHandler>,
) -> Result<(), std::io::Error> {
    let mut buffer = Vec::with_capacity(8 * 1024);
    let mut chunk = [0u8; 4096];
    let head_end = loop {
        let read = stream.read(&mut chunk).await?;
        if read == 0 {
            return Ok(());
        }
        buffer.extend_from_slice(&chunk[..read]);
        if let Some(position) = find_head_end(&buffer) {
            break position;
        }
        if buffer.len() > MAX_HEAD_BYTES {
            return respond(&mut stream, 431, "request head too large").await;
        }
    };

    let head = String::from_utf8_lossy(&buffer[..head_end]).into_owned();
    let Some((method, path, headers, declared)) = parse_head(&head) else {
        return respond(&mut stream, 400, "malformed request").await;
    };
    if method != "POST" && method != "GET" {
        return respond(&mut stream, 405, "method not allowed").await;
    }
    if declared > MAX_BODY_BYTES {
        return respond(&mut stream, 413, "body too large").await;
    }

    let mut body: Vec<u8> = buffer[head_end..].to_vec();
    while body.len() < declared {
        let read = stream.read(&mut chunk).await?;
        if read == 0 {
            break;
        }
        body.extend_from_slice(&chunk[..read]);
    }
    body.truncate(declared);

    let request = WebhookRequest {
        method,
        path,
        headers,
        body,
    };
    let (status, message) = hub.handle(request).await;
    respond(&mut stream, status, &message).await
}

fn find_head_end(buffer: &[u8]) -> Option<usize> {
    buffer
        .windows(4)
        .position(|window| window == b"\r\n\r\n")
        .map(|position| position + 4)
}

async fn respond(
    stream: &mut tokio::net::TcpStream,
    status: u16,
    body: &str,
) -> Result<(), std::io::Error> {
    let reason = match status {
        200 => "OK",
        202 => "Accepted",
        400 => "Bad Request",
        401 => "Unauthorized",
        403 => "Forbidden",
        404 => "Not Found",
        405 => "Method Not Allowed",
        413 => "Payload Too Large",
        431 => "Request Header Fields Too Large",
        _ => "Error",
    };
    let content_type = if body.trim_start().starts_with(['{', '[']) {
        "application/json"
    } else {
        "text/plain; charset=utf-8"
    };
    let response = format!(
        "HTTP/1.1 {status} {reason}\r\nContent-Type: {content_type}\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{body}",
        body.len()
    );
    stream.write_all(response.as_bytes()).await?;
    stream.shutdown().await
}

/// A platform whose events arrive over the ingress listener rather than a poll.
///
/// The HTTP route pushes parsed events into the channel this type drains; `send` goes through the
/// adapter exactly like a polled platform.
pub struct WebhookPlatform {
    name: String,
    sender: mpsc::Sender<MessageEvent>,
    receiver: Mutex<mpsc::Receiver<MessageEvent>>,
    adapter: Arc<dyn PlatformAdapter>,
}

impl WebhookPlatform {
    pub fn new(name: impl Into<String>, adapter: Arc<dyn PlatformAdapter>) -> Arc<Self> {
        let (sender, receiver) = mpsc::channel(64);
        Arc::new(Self {
            name: name.into(),
            sender,
            receiver: Mutex::new(receiver),
            adapter,
        })
    }

    /// The route handler's half: every event pushed here becomes one turn.
    pub fn sender(&self) -> mpsc::Sender<MessageEvent> {
        self.sender.clone()
    }
}

#[async_trait]
impl GatewayPlatform for WebhookPlatform {
    fn name(&self) -> &str {
        &self.name
    }

    async fn next_events(&self) -> Result<Vec<MessageEvent>, String> {
        let mut receiver = self.receiver.lock().await;
        match receiver.recv().await {
            Some(event) => Ok(vec![event]),
            None => Err("webhook channel closed".to_string()),
        }
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

/// Build a handler that verifies, parses, and forwards events to a platform's channel.
pub fn event_route<F>(handler: F) -> Arc<dyn WebhookHandler>
where
    F: Fn(WebhookRequest) -> Pin<Box<dyn Future<Output = (u16, String)> + Send>>
        + Send
        + Sync
        + 'static,
{
    Arc::new(EventRoute {
        handler: Box::new(handler),
    })
}

/// The boxed async handler a route is built from.
pub type RouteFn =
    dyn Fn(WebhookRequest) -> Pin<Box<dyn Future<Output = (u16, String)> + Send>> + Send + Sync;

struct EventRoute {
    handler: Box<RouteFn>,
}

#[async_trait]
impl WebhookHandler for EventRoute {
    async fn handle(&self, request: WebhookRequest) -> (u16, String) {
        (self.handler)(request).await
    }
}

/// One turn's reply for a webhook event; provided so routes do not need to import the host type.
pub type WebhookReply = ReplyFuture;

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_request_head_is_parsed_with_content_length() {
        let (method, path, headers, length) = parse_head(
            "POST /feishu HTTP/1.1\r\nHost: x\r\nContent-Length: 12\r\nContent-Type: application/json\r\n",
        )
        .expect("parsed");
        assert_eq!(method, "POST");
        assert_eq!(path, "/feishu");
        assert_eq!(length, 12);
        assert!(
            headers
                .iter()
                .any(|(name, value)| name == "Host" && value == "x")
        );
    }

    #[test]
    fn a_request_without_content_length_has_an_empty_body() {
        let (_, _, _, length) = parse_head("POST /x HTTP/1.1\r\nHost: y\r\n").unwrap();
        assert_eq!(length, 0);
    }

    #[test]
    fn malformed_heads_are_rejected() {
        assert!(parse_head("").is_none());
        assert!(parse_head("POST /x").is_none());
        assert!(parse_head("POST /x HTTP/1.1\r\nBroken-Header\r\n").is_none());
    }

    #[test]
    fn the_head_end_finds_the_blank_line() {
        assert_eq!(find_head_end(b"POST / HTTP/1.1\r\n\r\nbody"), Some(19));
        assert_eq!(find_head_end(b"POST / HTTP/1.1\r\n"), None);
    }

    #[tokio::test]
    async fn the_hub_routes_by_exact_path_and_404s_otherwise() {
        let hub = WebhookHub::new().route(
            "/ok",
            event_route(|request| {
                Box::pin(async move { (200, format!("got {} bytes", request.body.len())) })
            }),
        );
        let request = WebhookRequest {
            method: "POST".into(),
            path: "/ok".into(),
            headers: Vec::new(),
            body: b"abc".to_vec(),
        };
        assert_eq!(hub.handle(request).await, (200, "got 3 bytes".into()));

        let missing = WebhookRequest {
            method: "POST".into(),
            path: "/nope".into(),
            headers: Vec::new(),
            body: Vec::new(),
        };
        assert_eq!(hub.handle(missing).await.0, 404);
    }

    #[tokio::test]
    async fn a_webhook_platform_delivers_pushed_events() {
        struct Dummy;
        #[async_trait]
        impl PlatformAdapter for Dummy {
            async fn connect(&mut self, _: bool) -> bool {
                true
            }
            async fn disconnect(&mut self) {}
            async fn send(
                &self,
                chat_id: &str,
                content: &str,
                _: Option<&str>,
                _: Option<serde_json::Value>,
            ) -> SendResult {
                SendResult::ok(format!("{chat_id}:{content}"))
            }
            async fn get_chat_info(&self, chat_id: &str) -> crate::platform::ChatInfo {
                crate::platform::ChatInfo {
                    name: chat_id.into(),
                    chat_type: "dm".into(),
                }
            }
        }

        let platform = WebhookPlatform::new("dummy", Arc::new(Dummy));
        let sender = platform.sender();
        let event = MessageEvent::new(
            "hello",
            crate::event::SessionSource::new(
                crate::event::Platform::new("dummy").unwrap(),
                "chat-1",
            ),
        );
        sender.send(event).await.unwrap();
        let events = platform.next_events().await.unwrap();
        assert_eq!(events.len(), 1);
        assert_eq!(events[0].text, "hello");
        assert!(platform.send("chat-1", "reply").await.success);
    }
}
