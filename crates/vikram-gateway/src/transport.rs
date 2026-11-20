//! The one thing every platform adapter needs from the outside world: an HTTP request.
//!
//! Adapters never construct a `reqwest::Client` themselves. They build an [`HttpRequest`] and
//! hand it to a [`Transport`], which lets every adapter's wire behaviour be tested against a mock
//! transport with no network, no token, and no platform account.

use async_trait::async_trait;
use std::sync::Arc;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum HttpMethod {
    Get,
    Post,
    Put,
    Patch,
    Delete,
}

/// A request an adapter wants performed. Bodies are already encoded: JSON adapters pass bytes,
/// form adapters pass `application/x-www-form-urlencoded` bytes.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct HttpRequest {
    pub method: HttpMethod,
    pub url: String,
    pub headers: Vec<(String, String)>,
    pub body: Option<Vec<u8>>,
}

impl HttpRequest {
    pub fn get(url: impl Into<String>) -> Self {
        Self {
            method: HttpMethod::Get,
            url: url.into(),
            headers: Vec::new(),
            body: None,
        }
    }

    pub fn post(url: impl Into<String>) -> Self {
        Self {
            method: HttpMethod::Post,
            url: url.into(),
            headers: Vec::new(),
            body: None,
        }
    }

    pub fn put(url: impl Into<String>) -> Self {
        Self {
            method: HttpMethod::Put,
            url: url.into(),
            headers: Vec::new(),
            body: None,
        }
    }

    pub fn patch(url: impl Into<String>) -> Self {
        Self {
            method: HttpMethod::Patch,
            url: url.into(),
            headers: Vec::new(),
            body: None,
        }
    }

    pub fn delete(url: impl Into<String>) -> Self {
        Self {
            method: HttpMethod::Delete,
            url: url.into(),
            headers: Vec::new(),
            body: None,
        }
    }

    pub fn json(url: impl Into<String>, value: &serde_json::Value) -> Self {
        Self::post(url).json_body(value)
    }

    pub fn put_json(url: impl Into<String>, value: &serde_json::Value) -> Self {
        Self::put(url).json_body(value)
    }

    pub fn patch_json(url: impl Into<String>, value: &serde_json::Value) -> Self {
        Self::patch(url).json_body(value)
    }

    fn json_body(self, value: &serde_json::Value) -> Self {
        let body = serde_json::to_vec(value).unwrap_or_else(|_| b"{}".to_vec());
        self.header("Content-Type", "application/json").body(body)
    }

    pub fn form(url: impl Into<String>, pairs: &[(&str, String)]) -> Self {
        let encoded = pairs
            .iter()
            .map(|(key, value)| format!("{}={}", encode_component(key), encode_component(value)))
            .collect::<Vec<_>>()
            .join("&");
        Self::post(url)
            .header("Content-Type", "application/x-www-form-urlencoded")
            .body(encoded.into_bytes())
    }

    pub fn header(mut self, name: impl Into<String>, value: impl Into<String>) -> Self {
        self.headers.push((name.into(), value.into()));
        self
    }

    pub fn bearer(self, token: &str) -> Self {
        self.header("Authorization", format!("Bearer {token}"))
    }

    pub fn body(mut self, body: impl Into<Vec<u8>>) -> Self {
        self.body = Some(body.into());
        self
    }
}

/// Percent-encode one form component. Unreserved characters are kept; everything else is escaped.
fn encode_component(value: &str) -> String {
    let mut out = String::with_capacity(value.len());
    for byte in value.bytes() {
        match byte {
            b'A'..=b'Z' | b'a'..=b'z' | b'0'..=b'9' | b'-' | b'_' | b'.' | b'~' => {
                out.push(byte as char)
            }
            other => out.push_str(&format!("%{other:02X}")),
        }
    }
    out
}

#[derive(Clone, Debug, PartialEq)]
pub struct HttpResponse {
    pub status: u16,
    pub body: String,
    /// Seconds the server asked us to wait (`Retry-After` / Telegram `retry_after`).
    pub retry_after: Option<f64>,
}

impl HttpResponse {
    pub fn ok(&self) -> bool {
        (200..300).contains(&self.status)
    }

    pub fn json(&self) -> Result<serde_json::Value, TransportError> {
        serde_json::from_str(&self.body)
            .map_err(|err| TransportError::Json(format!("{err}: {}", truncate(&self.body, 200))))
    }
}

fn truncate(text: &str, limit: usize) -> String {
    if text.chars().count() <= limit {
        return text.to_string();
    }
    text.chars().take(limit).collect::<String>() + "…"
}

#[derive(Debug, thiserror::Error)]
pub enum TransportError {
    #[error("request failed: {0}")]
    Request(String),
    #[error("response was not json: {0}")]
    Json(String),
    #[error("response body is larger than the {0} byte limit")]
    TooLarge(usize),
}

#[async_trait]
pub trait Transport: Send + Sync {
    async fn execute(&self, request: HttpRequest) -> Result<HttpResponse, TransportError>;

    /// Perform a request and hand back the response body as the bytes that came off the wire.
    ///
    /// A text adapter only ever needs [`Transport::execute`], whose body is UTF-8 lossily
    /// reconciled from the wire. An image download cannot survive that: the bytes must arrive
    /// exactly as sent, so a caller that needs them (see [`crate::media_fetch`]) asks for them
    /// here. Status reporting is deliberately absent — the body is returned whatever the status
    /// was, exactly like `execute`, and it is the caller's job to decide whether the payload is
    /// acceptable.
    ///
    /// The default refuses. A transport that has not thought about binary bodies should fail
    /// loudly rather than hand back something that was never a byte-for-byte copy.
    ///
    /// `max_bytes` is a hard ceiling on the body. A transport that streams must abort the
    /// download as soon as the received bytes exceed it — buffering the whole response and
    /// checking afterwards lets a hostile server force an oversized allocation.
    async fn execute_bytes(
        &self,
        request: HttpRequest,
        max_bytes: usize,
    ) -> Result<Vec<u8>, TransportError> {
        let _ = (request, max_bytes);
        Err(TransportError::Request(
            "binary bodies are not supported by this transport".to_string(),
        ))
    }
}

pub type SharedTransport = Arc<dyn Transport>;

/// The real transport. Built once and shared by every adapter in the process.
pub struct ReqwestTransport {
    client: reqwest::Client,
    /// `execute_bytes` serves untrusted attachment URLs. It uses a client that does not follow
    /// redirects: the SSRF guard in `media_fetch` vets the URL it was handed, and a 302 would move
    /// the fetch to a host that was never checked. Its DNS resolver resolves each host once,
    /// refuses private answers, and dials only the vetted addresses — so a public name cannot
    /// rebind to a private address between the guard's check and the connect.
    bytes_client: reqwest::Client,
}

/// DNS resolver for the bytes client. Every answer is checked against the same private-address
/// rule `media_fetch` applies to the URL text; if any resolved address is private the whole
/// name is refused, and the surviving addresses are the only ones the connector dials.
struct VettedDns {
    allow_private: bool,
}

/// True when any resolved address is one `media_fetch` would refuse.
fn private_resolution(addrs: &[std::net::SocketAddr]) -> bool {
    addrs
        .iter()
        .any(|addr| crate::media_fetch::private_address(addr.ip()))
}

impl reqwest::dns::Resolve for VettedDns {
    fn resolve(&self, name: reqwest::dns::Name) -> reqwest::dns::Resolving {
        let host = name.as_str().to_string();
        let allow_private = self.allow_private;
        Box::pin(async move {
            let fail = |message: &str| {
                Box::new(std::io::Error::other(message.to_string()))
                    as Box<dyn std::error::Error + Send + Sync>
            };
            let addrs = tokio::net::lookup_host((host.as_str(), 0))
                .await
                .map_err(|err| fail(&err.to_string()))?
                .collect::<Vec<std::net::SocketAddr>>();
            if addrs.is_empty() {
                return Err(fail("the host resolved to no addresses"));
            }
            if !allow_private && private_resolution(&addrs) {
                return Err(fail("the host resolves to a private address"));
            }
            Ok(Box::new(addrs.into_iter()) as reqwest::dns::Addrs)
        })
    }
}

impl ReqwestTransport {
    pub fn new() -> Result<Self, TransportError> {
        let _ = rustls::crypto::ring::default_provider().install_default();
        Ok(Self {
            client: Self::build_client(true, false)?,
            bytes_client: Self::build_client(false, false)?,
        })
    }

    /// Let `execute_bytes` connect to hosts that resolve to private addresses. Off by default,
    /// mirroring `MediaFetchLimits::allow_private`: an operator talking to a local bridge opts
    /// in here as well as in the fetch limits.
    pub fn with_private_media(mut self) -> Result<Self, TransportError> {
        self.bytes_client = Self::build_client(false, true)?;
        Ok(self)
    }

    fn build_client(
        redirects: bool,
        media_private: bool,
    ) -> Result<reqwest::Client, TransportError> {
        let mut builder = reqwest::Client::builder()
            .use_preconfigured_tls(crate::tls_config())
            .connect_timeout(std::time::Duration::from_secs(10))
            .timeout(std::time::Duration::from_secs(60));
        if !redirects {
            builder = builder
                .redirect(reqwest::redirect::Policy::none())
                .dns_resolver(VettedDns {
                    allow_private: media_private,
                });
        }
        builder
            .build()
            .map_err(|err| TransportError::Request(err.to_string()))
    }

    pub fn into_shared(self) -> SharedTransport {
        Arc::new(self)
    }

    /// Build and send the request. Both body modes share this so verbs, headers and bodies can
    /// never drift between them.
    async fn send(
        &self,
        client: &reqwest::Client,
        request: HttpRequest,
    ) -> Result<reqwest::Response, TransportError> {
        let mut builder = match request.method {
            HttpMethod::Get => client.get(&request.url),
            HttpMethod::Post => client.post(&request.url),
            HttpMethod::Put => client.put(&request.url),
            HttpMethod::Patch => client.patch(&request.url),
            HttpMethod::Delete => client.delete(&request.url),
        };
        for (name, value) in &request.headers {
            builder = builder.header(name, value);
        }
        if let Some(body) = request.body {
            builder = builder.body(body);
        }
        builder.send().await.map_err(|err| {
            // `err` carries the request URL — which for Telegram/DingTalk embeds the bot token.
            // `without_url` keeps the failure reason and drops the credential.
            TransportError::Request(err.without_url().to_string())
        })
    }
}

#[async_trait]
impl Transport for ReqwestTransport {
    async fn execute(&self, request: HttpRequest) -> Result<HttpResponse, TransportError> {
        let response = self.send(&self.client, request).await?;
        let status = response.status().as_u16();
        let retry_after = response
            .headers()
            .get(reqwest::header::RETRY_AFTER)
            .and_then(|value| value.to_str().ok())
            .and_then(|value| value.trim().parse::<f64>().ok());
        let body = response
            .text()
            .await
            .map_err(|err| TransportError::Request(err.without_url().to_string()))?;
        Ok(HttpResponse {
            status,
            body,
            retry_after,
        })
    }

    async fn execute_bytes(
        &self,
        request: HttpRequest,
        max_bytes: usize,
    ) -> Result<Vec<u8>, TransportError> {
        let mut response = self.send(&self.bytes_client, request).await?;
        // Redirects are not followed for binary downloads; a 3xx here means the attachment URL
        // tried to move the fetch somewhere the caller's SSRF guard never vetted.
        if response.status().is_redirection() {
            return Err(TransportError::Request(
                "the server answered with a redirect, which is not followed".to_string(),
            ));
        }
        if let Some(length) = response.content_length()
            && length > max_bytes as u64
        {
            return Err(TransportError::TooLarge(max_bytes));
        }
        // Stream the body and stop the moment it crosses the cap: `bytes()` would buffer the
        // whole response first, letting a hostile server force an oversized allocation.
        let mut body = Vec::new();
        while let Some(chunk) = response
            .chunk()
            .await
            .map_err(|err| TransportError::Request(err.without_url().to_string()))?
        {
            if body.len().saturating_add(chunk.len()) > max_bytes {
                return Err(TransportError::TooLarge(max_bytes));
            }
            body.extend_from_slice(&chunk);
        }
        Ok(body)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use tokio::io::{AsyncReadExt, AsyncWriteExt};

    /// A one-shot HTTP/1.1 server: accepts one connection, answers it with `body` and hangs up.
    /// The port is chosen by the OS, so two tests can never collide.
    async fn serve_once(content_type: &str, body: &[u8]) -> std::net::SocketAddr {
        let listener = match tokio::net::TcpListener::bind("127.0.0.1:0").await {
            Ok(listener) => listener,
            Err(err) => unreachable!("the test server must bind a loopback port: {err}"),
        };
        let addr = listener
            .local_addr()
            .expect("a bound listener reports its address");
        let content_type = content_type.to_string();
        let body = body.to_vec();
        tokio::spawn(async move {
            let Ok((mut stream, _)) = listener.accept().await else {
                return;
            };
            // One read is enough: the request is a short GET whose headers fit in one segment.
            let mut buffer = vec![0u8; 2048];
            let _ = stream.read(&mut buffer).await;
            let head = format!(
                "HTTP/1.1 200 OK\r\nContent-Type: {content_type}\r\nContent-Length: {}\r\nConnection: close\r\n\r\n",
                body.len()
            );
            let _ = stream.write_all(head.as_bytes()).await;
            let _ = stream.write_all(&body).await;
            let _ = stream.flush().await;
        });
        addr
    }

    /// A PNG header plus two bytes that are not valid UTF-8, which is exactly the shape a text
    /// body would mangle and a binary body must preserve.
    fn raw_png() -> Vec<u8> {
        vec![
            0x89, b'P', b'N', b'G', 0x0d, 0x0a, 0x1a, 0x0a, 0x00, 0xff, 0x10,
        ]
    }

    #[tokio::test]
    async fn execute_bytes_returns_the_body_off_the_wire() {
        let png = raw_png();
        let addr = serve_once("image/png", &png).await;
        let transport = match ReqwestTransport::new() {
            Ok(transport) => transport,
            Err(err) => unreachable!("a client builds without a network: {err}"),
        };
        let fetched = transport
            .execute_bytes(
                HttpRequest::get(format!("http://{addr}/a.png")).header("Accept", "image/png"),
                1024,
            )
            .await;
        assert_eq!(fetched.ok(), Some(png));
    }

    #[tokio::test]
    async fn execute_bytes_aborts_once_the_body_crosses_the_cap() {
        let body = vec![0x89u8; 8192];
        let addr = serve_once("image/png", &body).await;
        let transport = match ReqwestTransport::new() {
            Ok(transport) => transport,
            Err(err) => unreachable!("a client builds without a network: {err}"),
        };
        let result = transport
            .execute_bytes(HttpRequest::get(format!("http://{addr}/a.png")), 1024)
            .await;
        assert!(matches!(result, Err(TransportError::TooLarge(1024))));
    }

    #[tokio::test]
    async fn execute_bytes_rejects_a_declared_length_over_the_cap_before_reading() {
        // The server advertises far more than the cap and then closes; the transport must fail
        // on the Content-Length alone, without waiting for a body that never fully arrives.
        let listener = match tokio::net::TcpListener::bind("127.0.0.1:0").await {
            Ok(listener) => listener,
            Err(err) => unreachable!("the test server must bind a loopback port: {err}"),
        };
        let addr = listener
            .local_addr()
            .expect("a bound listener reports its address");
        tokio::spawn(async move {
            let Ok((mut stream, _)) = listener.accept().await else {
                return;
            };
            let mut buffer = vec![0u8; 2048];
            let _ = stream.read(&mut buffer).await;
            let _ = stream
                .write_all(
                    b"HTTP/1.1 200 OK\r\nContent-Length: 1048576\r\nConnection: close\r\n\r\n",
                )
                .await;
            let _ = stream.write_all(b"tiny").await;
            let _ = stream.flush().await;
        });
        let transport = match ReqwestTransport::new() {
            Ok(transport) => transport,
            Err(err) => unreachable!("a client builds without a network: {err}"),
        };
        let result = transport
            .execute_bytes(HttpRequest::get(format!("http://{addr}/a.png")), 1024)
            .await;
        assert!(matches!(result, Err(TransportError::TooLarge(1024))));
    }

    #[tokio::test]
    async fn execute_still_reads_text_from_the_same_transport() {
        let png = raw_png();
        let addr = serve_once("image/png", &png).await;
        let transport = match ReqwestTransport::new() {
            Ok(transport) => transport,
            Err(err) => unreachable!("a client builds without a network: {err}"),
        };
        let response = transport
            .execute(HttpRequest::get(format!("http://{addr}/a.png")))
            .await
            .expect("the server answers");
        assert_eq!(response.status, 200);
        assert!(response.ok());
        // Text mode is unchanged: the body is a string, and the non-UTF-8 byte became a
        // replacement character rather than surviving as-is.
        assert!(response.body.contains("PNG"));
        assert!(response.body.contains('\u{fffd}'));
    }

    /// A transport that only ever needed text.
    struct TextOnly;

    #[async_trait]
    impl Transport for TextOnly {
        async fn execute(&self, request: HttpRequest) -> Result<HttpResponse, TransportError> {
            Ok(HttpResponse {
                status: 200,
                body: request.url,
                retry_after: None,
            })
        }
    }

    #[tokio::test]
    async fn a_transport_without_a_binary_body_refuses_instead_of_guessing() {
        let result = TextOnly
            .execute_bytes(HttpRequest::get("https://example.test/a.png"), 1024)
            .await;
        assert_eq!(
            result.err().map(|err| err.to_string()),
            Some("request failed: binary bodies are not supported by this transport".to_string())
        );
    }

    #[test]
    fn the_full_verb_set_has_a_json_helper_and_a_constructor() {
        for (request, method) in [
            (HttpRequest::get("https://x"), HttpMethod::Get),
            (HttpRequest::post("https://x"), HttpMethod::Post),
            (HttpRequest::put("https://x"), HttpMethod::Put),
            (HttpRequest::patch("https://x"), HttpMethod::Patch),
            (HttpRequest::delete("https://x"), HttpMethod::Delete),
        ] {
            assert_eq!(request.method, method);
        }
        let put = HttpRequest::put_json("https://x", &serde_json::json!({"a": 1}));
        assert_eq!(put.method, HttpMethod::Put);
        assert!(put.headers.iter().any(|(name, _)| name == "Content-Type"));
        let patch = HttpRequest::patch_json("https://x", &serde_json::json!({}));
        assert_eq!(patch.method, HttpMethod::Patch);
    }

    #[test]
    fn json_requests_set_the_content_type_and_encode_the_body() {
        let request = HttpRequest::json("https://example.test/x", &serde_json::json!({"a": 1}));
        assert_eq!(request.method, HttpMethod::Post);
        assert!(
            request
                .headers
                .contains(&("Content-Type".to_string(), "application/json".to_string()))
        );
        let body: serde_json::Value =
            serde_json::from_slice(request.body.as_ref().expect("body")).expect("json");
        assert_eq!(body["a"], 1);
    }

    #[test]
    fn form_requests_percent_encode_reserved_characters() {
        let request = HttpRequest::form(
            "https://example.test/x",
            &[
                ("text", "a b&c".to_string()),
                ("chat_id", "-100".to_string()),
            ],
        );
        let body = String::from_utf8(request.body.expect("body")).expect("utf8");
        assert_eq!(body, "text=a%20b%26c&chat_id=-100");
    }

    #[test]
    fn bearer_and_headers_accumulate() {
        let request = HttpRequest::get("https://x")
            .bearer("t")
            .header("X-Test", "1");
        assert_eq!(request.headers[0].0, "Authorization");
        assert_eq!(request.headers[1].0, "X-Test");
    }

    #[test]
    fn responses_report_success_by_status() {
        for status in [200, 201, 204] {
            let response = HttpResponse {
                status,
                body: String::new(),
                retry_after: None,
            };
            assert!(response.ok(), "{status}");
        }
        for status in [400, 401, 429, 500] {
            let response = HttpResponse {
                status,
                body: String::new(),
                retry_after: None,
            };
            assert!(!response.ok(), "{status}");
        }
    }

    #[test]
    fn a_resolution_is_refused_when_any_answer_is_private() {
        let public = std::net::SocketAddr::from(([8, 8, 8, 8], 443));
        let private_v4 = std::net::SocketAddr::from(([10, 0, 0, 1], 443));
        let private_v6 = std::net::SocketAddr::from(([0, 0, 0, 0, 0, 0, 0, 1], 443));
        assert!(!private_resolution(&[public]));
        assert!(private_resolution(&[public, private_v4]));
        assert!(private_resolution(&[private_v6]));
        assert!(!private_resolution(&[]));
    }

    /// `localhost` resolves through the OS resolver to loopback in every environment that can
    /// run this test, so it exercises the real lookup path without touching the network.
    #[tokio::test]
    async fn the_dns_guard_refuses_a_name_that_resolves_private() {
        use reqwest::dns::{Name, Resolve};
        use std::str::FromStr;

        let name = Name::from_str("localhost").expect("localhost is a valid name");
        let strict = VettedDns {
            allow_private: false,
        };
        assert!(strict.resolve(name).await.is_err());

        let lenient = VettedDns {
            allow_private: true,
        };
        let name = Name::from_str("localhost").expect("localhost is a valid name");
        let addrs = lenient
            .resolve(name)
            .await
            .expect("an opted-in resolver returns the loopback answers")
            .collect::<Vec<_>>();
        assert!(!addrs.is_empty());
        assert!(addrs.iter().all(|addr| addr.ip().is_loopback()));
    }
}
