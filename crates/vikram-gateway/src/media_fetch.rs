//! Fetching an inbound chat attachment into something a model can be shown.
//!
//! Adapters record `media_urls` / `media_types` on a [`MessageEvent`](crate::event::MessageEvent),
//! but a gateway turn used to be text-only: the attachment was parsed and then dropped on the
//! floor, so the model never saw the picture the operator sent. This module closes that gap. It
//! turns the URLs of one event into inline base64 images, which the host attaches to the turn's
//! message as [`ImageSource`](vikram_contracts::ImageSource) `Data` blocks.
//!
//! Everything here treats the URL and the response as hostile input:
//!
//! - **Scheme.** Only `http` and `https`. A `file:`, `ftp:` or `mailto:` URL is refused before any
//!   transport is asked for it.
//! - **Server-side request forgery.** The gateway runs next to the runtime, so a peer that can
//!   make it fetch `http://169.254.169.254/…` or `http://10.0.0.5:6379/` turns the assistant into
//!   a proxy for the local network. [`MediaFetchLimits::allow_private`] is off by default and the
//!   guard refuses `localhost`, `*.localhost`, `*.local`, `*.internal`, and every literal
//!   loopback, private, link-local and ULA address in both families — unless an operator has
//!   explicitly opted in.
//! - **Content.** The declared `Content-Type` is ignored. The type is derived from the payload's
//!   magic bytes; anything that is not a PNG, JPEG, GIF or WebP is skipped, so an error page or a
//!   redirect body never reaches a model as an "image".
//! - **Size.** At most [`MediaFetchLimits::max_items`] images, each at most
//!   [`MediaFetchLimits::max_bytes`] bytes. A URL that fails, is too large, is empty or is not an
//!   image costs only that URL: the caller still gets every image that did work, and the reason
//!   for each skip.
//!
//! Limitations, stated so they are not mistaken for guarantees:
//!
//! - The guard resolves nothing. A public hostname that resolves to `10.0.0.1` at connect time
//!   (**DNS rebinding**) gets through; defeating that needs a resolver hooked into the connection,
//!   which is out of scope for this simple lexical guard.
//! - Redirects are followed by the underlying HTTP client. A public URL that redirects to a
//!   private one is not re-checked. Closing that needs a per-request redirect policy on the
//!   transport, which would change the behaviour every adapter shares.
//! - Only still images are fetched. Video, audio, PDFs and platform-specific sticker/animation
//!   payloads are skipped even when the adapter recorded a URL for them.
//! - The whole body is buffered by the transport before the byte cap is applied, so a hostile
//!   server can still make the process allocate one oversized response before it is discarded.

use vikram_contracts::ImageSource;

use crate::transport::{HttpRequest, Transport};

/// How much of an event's media one turn is allowed to inline.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct MediaFetchLimits {
    /// At most this many images are returned.
    pub max_items: usize,
    /// Per-image ceiling, in bytes, on the payload as received.
    pub max_bytes: usize,
    /// Fetch from loopback / private / link-local / ULA addresses. Off by default; only an
    /// operator who is deliberately talking to a local bridge should turn it on.
    pub allow_private: bool,
}

impl Default for MediaFetchLimits {
    fn default() -> Self {
        Self {
            max_items: 4,
            max_bytes: 10 * 1024 * 1024,
            allow_private: false,
        }
    }
}

/// One attachment that is ready to be inlined into a turn.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct FetchedImage {
    /// The type sniffed from the payload, never the one the server declared.
    pub mime: String,
    /// Standard, padded base64 of the raw bytes (no `data:` prefix).
    pub base64: String,
}

impl FetchedImage {
    /// The [`ImageSource`] an image travels as: inline data, because the provider may not be able
    /// to reach the platform's private or expiring attachment URL.
    pub fn into_source(self) -> ImageSource {
        ImageSource::data(self.mime, self.base64)
    }
}

/// Fetch the images behind `urls`, reporting what was skipped and why.
///
/// The reasons never contain the URL or any part of a payload: an attachment URL is routinely a
/// signed, expiring one, and this string ends up on a terminal. They are safe to log.
pub async fn fetch_images_with_skips(
    transport: &dyn Transport,
    urls: &[String],
    limits: &MediaFetchLimits,
) -> (Vec<FetchedImage>, Vec<String>) {
    let mut images = Vec::new();
    let mut skipped = Vec::new();
    for url in urls {
        if images.len() >= limits.max_items {
            skipped.push(format!(
                "only the first {} attachment(s) are inlined",
                limits.max_items
            ));
            break;
        }
        match fetch_one(transport, url, limits).await {
            Ok(image) => images.push(image),
            Err(reason) => skipped.push(reason),
        }
    }
    (images, skipped)
}

/// Fetch the images behind `urls`, keeping only the ones that worked.
pub async fn fetch_images(
    transport: &dyn Transport,
    urls: &[String],
    limits: &MediaFetchLimits,
) -> Vec<FetchedImage> {
    fetch_images_with_skips(transport, urls, limits).await.0
}

/// Fetch one URL and vouch for the result. `Err` carries a reason safe to log.
async fn fetch_one(
    transport: &dyn Transport,
    url: &str,
    limits: &MediaFetchLimits,
) -> Result<FetchedImage, String> {
    let host = host_of(url)?;
    if !limits.allow_private
        && let Some(reason) = private_target(&host)
    {
        return Err(reason.to_string());
    }
    let request =
        HttpRequest::get(url).header("Accept", "image/png,image/jpeg,image/gif,image/webp");
    // The transport's error text can quote the URL (query string included), so it is replaced
    // rather than forwarded.
    let body = transport
        .execute_bytes(request)
        .await
        .map_err(|_| "the attachment could not be downloaded".to_string())?;
    if body.is_empty() {
        return Err("the attachment was empty".to_string());
    }
    if body.len() > limits.max_bytes {
        return Err(format!(
            "the attachment is larger than the {} byte limit",
            limits.max_bytes
        ));
    }
    let mime = sniff_image(&body)
        .ok_or_else(|| "the attachment is not a png, jpeg, gif or webp image".to_string())?;
    Ok(FetchedImage {
        mime: mime.to_string(),
        base64: base64_encode(&body),
    })
}

/// The lowercased host of an `http`/`https` URL: no userinfo, no port, no IPv6 brackets.
fn host_of(url: &str) -> Result<String, String> {
    let (scheme, rest) = url
        .split_once("://")
        .ok_or_else(|| "the attachment URL is not absolute".to_string())?;
    if !scheme.eq_ignore_ascii_case("http") && !scheme.eq_ignore_ascii_case("https") {
        return Err("only http and https attachments are fetched".to_string());
    }
    // The authority ends where the path, query or fragment begins.
    let end = rest.find(['/', '?', '#']).unwrap_or(rest.len());
    let authority = rest.get(..end).unwrap_or_default();
    // Userinfo is not part of the host; take everything after the last `@`.
    let host_port = match authority.rsplit_once('@') {
        Some((_, after)) => after,
        None => authority,
    };
    if let Some(bracketed) = host_port.strip_prefix('[') {
        let close = bracketed
            .find(']')
            .ok_or_else(|| "the attachment URL has a malformed IPv6 host".to_string())?;
        let host = bracketed.get(..close).unwrap_or_default();
        if host.is_empty() {
            return Err("the attachment URL has no host".to_string());
        }
        return Ok(host.to_ascii_lowercase());
    }
    // A trailing `:port` is dropped; a colon that is not followed by digits is not a port.
    let host = match host_port.rsplit_once(':') {
        Some((host, port)) if !port.is_empty() && port.chars().all(|c| c.is_ascii_digit()) => host,
        _ => host_port,
    };
    if host.is_empty() {
        return Err("the attachment URL has no host".to_string());
    }
    Ok(host.to_ascii_lowercase())
}

/// Why a host must not be fetched, or `None` when it is a public name or address.
///
/// This is a lexical check: it sees the text of the URL, not what the resolver will answer. See
/// the module docs on DNS rebinding.
fn private_target(host: &str) -> Option<&'static str> {
    if host == "localhost" || host.ends_with(".localhost") {
        return Some("localhost attachments are refused");
    }
    if host.ends_with(".local") {
        return Some("*.local attachments are refused");
    }
    // Non-literal names like `metadata.internal` are the cloud-metadata convention.
    if host.ends_with(".internal") {
        return Some("*.internal attachments are refused");
    }
    if let Ok(address) = host.parse::<std::net::IpAddr>() {
        return if private_address(address) {
            Some("private, loopback or link-local attachments are refused")
        } else {
            None
        };
    }
    // `http://2130706433/` and `http://1.2.3/` are hostnames to this parser but addresses to a
    // resolver, so a numeric-looking name that is not a plain address is refused outright.
    if looks_numeric(host) {
        return Some("numeric host names are refused");
    }
    None
}

fn looks_numeric(host: &str) -> bool {
    let labels = host
        .split('.')
        .filter(|label| !label.is_empty())
        .collect::<Vec<_>>();
    !labels.is_empty() && labels.iter().all(|label| numeric_label(label))
}

/// `123`, `0x7f` — the two spellings a resolver may read as one octet of an address.
fn numeric_label(label: &str) -> bool {
    if let Some(digits) = label
        .strip_prefix("0x")
        .or_else(|| label.strip_prefix("0X"))
    {
        return !digits.is_empty() && digits.chars().all(|c| c.is_ascii_hexdigit());
    }
    !label.is_empty() && label.chars().all(|c| c.is_ascii_digit())
}

fn private_address(address: std::net::IpAddr) -> bool {
    match address {
        std::net::IpAddr::V4(v4) => {
            v4.is_loopback()
                || v4.is_private()
                || v4.is_link_local()
                || v4.is_unspecified()
                || v4.is_broadcast()
                || v4.octets()[0] == 0
        }
        std::net::IpAddr::V6(v6) => {
            if v6.is_loopback() || v6.is_unspecified() {
                return true;
            }
            // `::ffff:10.0.0.1` is a private IPv4 address wearing an IPv6 hat.
            if let Some(v4) = v6.to_ipv4_mapped() {
                return private_address(std::net::IpAddr::V4(v4));
            }
            let segments = v6.segments();
            (segments[0] & 0xfe00) == 0xfc00 // fc00::/7, unique local
                || (segments[0] & 0xffc0) == 0xfe80 // fe80::/10, link local
        }
    }
}

/// The MIME type of an image payload, from its magic bytes only.
fn sniff_image(bytes: &[u8]) -> Option<&'static str> {
    const PNG: [u8; 8] = [0x89, b'P', b'N', b'G', 0x0d, 0x0a, 0x1a, 0x0a];
    if bytes.starts_with(&PNG) {
        return Some("image/png");
    }
    if bytes.starts_with(&[0xff, 0xd8, 0xff]) {
        return Some("image/jpeg");
    }
    if bytes.starts_with(b"GIF87a") || bytes.starts_with(b"GIF89a") {
        return Some("image/gif");
    }
    if bytes.len() >= 12 && &bytes[..4] == b"RIFF" && &bytes[8..12] == b"WEBP" {
        return Some("image/webp");
    }
    None
}

/// Standard, padded base64 (RFC 4648 §4). Hand-rolled because this crate may not add a
/// dependency for sixteen lines of arithmetic.
fn base64_encode(data: &[u8]) -> String {
    const ALPHABET: &[u8; 64] = b"ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz0123456789+/";
    let mut out = String::with_capacity(data.len().div_ceil(3) * 4);
    for chunk in data.chunks(3) {
        let first = chunk.first().copied().unwrap_or(0) as u32;
        let second = chunk.get(1).copied().unwrap_or(0) as u32;
        let third = chunk.get(2).copied().unwrap_or(0) as u32;
        let triple = (first << 16) | (second << 8) | third;
        out.push(ALPHABET[(triple >> 18) as usize & 0x3f] as char);
        out.push(ALPHABET[(triple >> 12) as usize & 0x3f] as char);
        if chunk.len() > 1 {
            out.push(ALPHABET[(triple >> 6) as usize & 0x3f] as char);
        } else {
            out.push('=');
        }
        if chunk.len() > 2 {
            out.push(ALPHABET[triple as usize & 0x3f] as char);
        } else {
            out.push('=');
        }
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::transport::{HttpResponse, TransportError};
    use async_trait::async_trait;
    use std::collections::BTreeMap;
    use std::sync::atomic::{AtomicUsize, Ordering};

    /// The four formats the sniffer knows, with the smallest payload that identifies each.
    fn png() -> Vec<u8> {
        vec![0x89, b'P', b'N', b'G', 0x0d, 0x0a, 0x1a, 0x0a, 0x00, 0x01]
    }

    fn jpeg() -> Vec<u8> {
        vec![0xff, 0xd8, 0xff, 0xe0, 0x00, 0x10]
    }

    fn gif() -> Vec<u8> {
        b"GIF89a\x01\x00\x01\x00".to_vec()
    }

    fn webp() -> Vec<u8> {
        let mut bytes = b"RIFF".to_vec();
        bytes.extend_from_slice(&[0x24, 0x00, 0x00, 0x00]);
        bytes.extend_from_slice(b"WEBPVP8 ");
        bytes
    }

    /// A transport that answers with the body registered for a URL and counts the requests it
    /// was asked to make. A URL with no registered body answers with an error, like a 404 or a
    /// connection that dropped.
    struct MockTransport {
        bodies: BTreeMap<String, Vec<u8>>,
        requests: AtomicUsize,
    }

    impl MockTransport {
        fn with(pairs: &[(&str, Vec<u8>)]) -> Self {
            Self {
                bodies: pairs
                    .iter()
                    .map(|(url, body)| ((*url).to_string(), body.clone()))
                    .collect(),
                requests: AtomicUsize::new(0),
            }
        }

        fn calls(&self) -> usize {
            self.requests.load(Ordering::SeqCst)
        }
    }

    #[async_trait]
    impl Transport for MockTransport {
        async fn execute(&self, _request: HttpRequest) -> Result<HttpResponse, TransportError> {
            Err(TransportError::Request(
                "the mock only speaks bytes".to_string(),
            ))
        }

        async fn execute_bytes(&self, request: HttpRequest) -> Result<Vec<u8>, TransportError> {
            self.requests.fetch_add(1, Ordering::SeqCst);
            match self.bodies.get(&request.url) {
                Some(body) => Ok(body.clone()),
                None => Err(TransportError::Request("404 not found".to_string())),
            }
        }
    }

    fn urls(list: &[&str]) -> Vec<String> {
        list.iter().map(|url| (*url).to_string()).collect()
    }

    #[test]
    fn base64_matches_the_rfc_4648_vectors() {
        // RFC 4648 §10, the canonical test vectors, padded to a multiple of four.
        for (input, expected) in [
            ("", ""),
            ("f", "Zg=="),
            ("fo", "Zm8="),
            ("foo", "Zm9v"),
            ("foob", "Zm9vYg=="),
            ("fooba", "Zm9vYmE="),
            ("foobar", "Zm9vYmFy"),
        ] {
            assert_eq!(base64_encode(input.as_bytes()), expected, "{input:?}");
        }
        // A full 0x00..=0xff sweep exercises every fragment length in one pass.
        let all: Vec<u8> = (0..=255u8).collect();
        let encoded = base64_encode(&all);
        assert_eq!(encoded.len(), 344);
        assert!(encoded.starts_with("AAECAwQFBgcICQoLDA0ODxAREhMUFRYXGBkaGxwdHh8gISIj"));
        assert!(encoded.ends_with("+fr7/P3+/w=="));
    }

    #[test]
    fn the_sniffer_reads_each_format_and_refuses_anything_else() {
        assert_eq!(sniff_image(&png()), Some("image/png"));
        assert_eq!(sniff_image(&jpeg()), Some("image/jpeg"));
        assert_eq!(sniff_image(&gif()), Some("image/gif"));
        assert_eq!(sniff_image(&webp()), Some("image/webp"));
        // GIF87a is the older spelling of the same format.
        assert_eq!(sniff_image(b"GIF87a......"), Some("image/gif"));
        // An HTML error page, a JSON body, a RIFF WAV file, a truncated header and nothing at all.
        assert_eq!(sniff_image(b"<!doctype html><h1>404</h1>"), None);
        assert_eq!(sniff_image(b"{\"error\":\"gone\"}"), None);
        let mut wav = b"RIFF".to_vec();
        wav.extend_from_slice(&[0x24, 0x00, 0x00, 0x00]);
        wav.extend_from_slice(b"WAVEfmt ");
        assert_eq!(sniff_image(&wav), None);
        assert_eq!(sniff_image(&[0x89, b'P']), None);
        assert_eq!(sniff_image(b""), None);
    }

    #[test]
    fn the_host_is_taken_from_the_url_without_userinfo_port_or_brackets() {
        assert_eq!(
            host_of("https://user:secret@cdn.example.test:8443/a.png?x=1"),
            Ok("cdn.example.test".to_string())
        );
        assert_eq!(
            host_of("HTTPS://Example.TEST/a"),
            Ok("example.test".to_string())
        );
        assert_eq!(host_of("http://[::1]:8080/a.png"), Ok("::1".to_string()));
        assert_eq!(
            host_of("http://example.test:443"),
            Ok("example.test".to_string())
        );
        assert!(host_of("file:///etc/passwd").is_err());
        assert!(host_of("/relative/a.png").is_err());
        assert!(host_of("http:///a.png").is_err());
        assert!(host_of("http://[::1/a.png").is_err());
    }

    #[tokio::test]
    async fn every_private_and_loopback_form_is_refused_before_a_request_is_made() {
        let refused = [
            // IPv4: RFC 1918, link-local, loopback, this-network, broadcast.
            "http://127.0.0.1/a.png",
            "http://127.255.255.254/a.png",
            "http://10.0.0.1/a.png",
            "http://10.255.255.255/a.png",
            "http://172.16.0.1/a.png",
            "http://172.31.255.255/a.png",
            "http://192.168.0.1/a.png",
            "http://169.254.169.254/latest/meta-data/",
            "http://0.0.0.0/a.png",
            "http://255.255.255.255/a.png",
            // IPv6: loopback, unique-local, link-local, and IPv4-mapped privates.
            "http://[::1]/a.png",
            "http://[fc00::1]/a.png",
            "http://[fd12:3456:789a::1]/a.png",
            "http://[fe80::1]/a.png",
            "http://[::ffff:127.0.0.1]/a.png",
            "http://[::ffff:10.1.2.3]/a.png",
            // Names.
            "http://localhost/a.png",
            "http://LOCALHOST/a.png",
            "http://api.localhost/a.png",
            "http://printer.local/a.png",
            "http://metadata.internal/a.png",
            "http://svc.cluster.internal/a.png",
            // Encodings a resolver may read as an address.
            "http://2130706433/a.png",
            "http://0x7f.0.0.1/a.png",
            // Schemes that never leave the machine or are not HTTP at all.
            "file:///etc/passwd",
            "ftp://example.test/a.png",
            "mailto:someone@example.test",
            "gopher://example.test/a.png",
            "data:image/png;base64,iVBORw0KGgo=",
        ];
        // A public host is registered so the guard cannot pass by accident: if a "private" URL
        // slipped through it would be fetched, fail the lookup and show up as a skip anyway. The
        // request counter is what actually proves nothing was asked for.
        let transport = MockTransport::with(&[("https://cdn.example.test/a.png", png())]);
        for url in refused {
            let (images, skipped) =
                fetch_images_with_skips(&transport, &urls(&[url]), &MediaFetchLimits::default())
                    .await;
            assert!(images.is_empty(), "{url} produced an image");
            assert_eq!(skipped.len(), 1, "{url} was not reported");
            assert!(!skipped[0].is_empty(), "{url} skipped without a reason");
        }
        assert_eq!(transport.calls(), 0, "a refused URL must not be fetched");

        // The positive control: the same fetch machinery does work on a public host, so the
        // refusals above are the guard's doing and not a broken mock.
        let allowed = fetch_images(
            &transport,
            &urls(&["https://cdn.example.test/a.png"]),
            &MediaFetchLimits::default(),
        )
        .await;
        assert_eq!(allowed.len(), 1);
        assert_eq!(transport.calls(), 1);
    }

    #[tokio::test]
    async fn a_public_https_url_is_fetched_and_sniffed() {
        let transport = MockTransport::with(&[("https://cdn.example.test/a.png", png())]);
        let images = fetch_images(
            &transport,
            &urls(&["https://cdn.example.test/a.png"]),
            &MediaFetchLimits::default(),
        )
        .await;
        assert_eq!(images.len(), 1);
        assert_eq!(images[0].mime, "image/png");
        assert_eq!(images[0].base64, base64_encode(&png()));
        assert_eq!(
            images[0].clone().into_source(),
            ImageSource::data("image/png", base64_encode(&png()))
        );
    }

    #[tokio::test]
    async fn the_declared_content_type_is_ignored_in_favour_of_the_magic_bytes() {
        // Every URL was recorded as an image by its adapter; only the payload decides.
        let transport = MockTransport::with(&[
            ("https://cdn.example.test/a", png()),
            ("https://cdn.example.test/b", jpeg()),
            ("https://cdn.example.test/c", gif()),
            ("https://cdn.example.test/d", webp()),
            ("https://cdn.example.test/e", b"<html>404</html>".to_vec()),
        ]);
        let (images, skipped) = fetch_images_with_skips(
            &transport,
            &urls(&[
                "https://cdn.example.test/a",
                "https://cdn.example.test/b",
                "https://cdn.example.test/c",
                "https://cdn.example.test/d",
                "https://cdn.example.test/e",
            ]),
            // Five URLs, so the item cap is lifted: this test is about the payload, not the count.
            &MediaFetchLimits {
                max_items: 8,
                ..MediaFetchLimits::default()
            },
        )
        .await;
        assert_eq!(
            images
                .iter()
                .map(|image| image.mime.as_str())
                .collect::<Vec<_>>(),
            vec!["image/png", "image/jpeg", "image/gif", "image/webp"]
        );
        assert_eq!(skipped.len(), 1);
        assert!(skipped[0].contains("not a png"));
    }

    #[tokio::test]
    async fn an_empty_body_is_skipped() {
        let transport = MockTransport::with(&[("https://cdn.example.test/a.png", Vec::new())]);
        let (images, skipped) = fetch_images_with_skips(
            &transport,
            &urls(&["https://cdn.example.test/a.png"]),
            &MediaFetchLimits::default(),
        )
        .await;
        assert!(images.is_empty());
        assert_eq!(skipped, vec!["the attachment was empty".to_string()]);
    }

    #[tokio::test]
    async fn an_oversized_body_is_skipped_and_the_next_one_still_arrives() {
        let transport = MockTransport::with(&[
            ("https://cdn.example.test/big.png", png()),
            // Eight bytes: the shortest payload that still sniffs as a PNG.
            ("https://cdn.example.test/small.png", png()[..8].to_vec()),
        ]);
        let limits = MediaFetchLimits {
            max_bytes: 9,
            ..MediaFetchLimits::default()
        };
        let (images, skipped) = fetch_images_with_skips(
            &transport,
            &urls(&[
                "https://cdn.example.test/big.png",
                "https://cdn.example.test/small.png",
            ]),
            &limits,
        )
        .await;
        assert_eq!(images.len(), 1, "the small image still arrives");
        assert_eq!(images[0].mime, "image/png");
        assert_eq!(
            skipped,
            vec!["the attachment is larger than the 9 byte limit".to_string()]
        );
    }

    #[tokio::test]
    async fn the_item_cap_stops_the_fetch_and_says_so() {
        let transport = MockTransport::with(&[
            ("https://cdn.example.test/1.png", png()),
            ("https://cdn.example.test/2.png", png()),
            ("https://cdn.example.test/3.png", png()),
        ]);
        let limits = MediaFetchLimits {
            max_items: 2,
            ..MediaFetchLimits::default()
        };
        let (images, skipped) = fetch_images_with_skips(
            &transport,
            &urls(&[
                "https://cdn.example.test/1.png",
                "https://cdn.example.test/2.png",
                "https://cdn.example.test/3.png",
            ]),
            &limits,
        )
        .await;
        assert_eq!(images.len(), 2);
        assert_eq!(transport.calls(), 2, "the third URL is never fetched");
        assert_eq!(skipped.len(), 1);
        assert!(skipped[0].contains("only the first 2 attachment(s)"));

        // Zero is a legal policy too: nothing is fetched at all.
        let none = MediaFetchLimits {
            max_items: 0,
            ..MediaFetchLimits::default()
        };
        let (images, skipped) = fetch_images_with_skips(
            &transport,
            &urls(&["https://cdn.example.test/1.png"]),
            &none,
        )
        .await;
        assert!(images.is_empty());
        assert_eq!(skipped.len(), 1);
        assert_eq!(transport.calls(), 2);
    }

    #[tokio::test]
    async fn a_failed_response_skips_only_that_url() {
        // `missing.png` is not registered, so the mock answers with a transport error, which is
        // what a 404 or a dropped connection looks like to this layer.
        let transport = MockTransport::with(&[("https://cdn.example.test/ok.png", png())]);
        let (images, skipped) = fetch_images_with_skips(
            &transport,
            &urls(&[
                "https://cdn.example.test/missing.png",
                "https://cdn.example.test/ok.png",
            ]),
            &MediaFetchLimits::default(),
        )
        .await;
        assert_eq!(images.len(), 1);
        assert_eq!(
            skipped,
            vec!["the attachment could not be downloaded".to_string()]
        );
        assert_eq!(transport.calls(), 2);
    }

    #[tokio::test]
    async fn allow_private_lets_an_operator_reach_a_local_bridge() {
        let transport = MockTransport::with(&[("http://127.0.0.1:9000/a.png", png())]);
        let denied = fetch_images(
            &transport,
            &urls(&["http://127.0.0.1:9000/a.png"]),
            &MediaFetchLimits::default(),
        )
        .await;
        assert!(denied.is_empty());
        assert_eq!(transport.calls(), 0);

        let limits = MediaFetchLimits {
            allow_private: true,
            ..MediaFetchLimits::default()
        };
        let allowed =
            fetch_images(&transport, &urls(&["http://127.0.0.1:9000/a.png"]), &limits).await;
        assert_eq!(allowed.len(), 1);
        assert_eq!(allowed[0].mime, "image/png");
        assert_eq!(transport.calls(), 1);
    }

    #[tokio::test]
    async fn no_urls_means_no_requests() {
        let transport = MockTransport::with(&[]);
        let (images, skipped) =
            fetch_images_with_skips(&transport, &[], &MediaFetchLimits::default()).await;
        assert!(images.is_empty());
        assert!(skipped.is_empty());
        assert_eq!(transport.calls(), 0);
    }

    #[test]
    fn the_default_limits_are_small_enough_to_be_safe() {
        let limits = MediaFetchLimits::default();
        assert_eq!(limits.max_items, 4);
        assert_eq!(limits.max_bytes, 10 * 1024 * 1024);
        assert!(!limits.allow_private);
    }
}
