//! Just enough HTTP/1.1 to serve eight routes.
//!
//! The request is read one byte at a time up to the blank line. That is deliberate: a WebSocket
//! client is allowed to send its first frame immediately after the upgrade request, and a buffered
//! reader would swallow those bytes and hand tungstenite a stream that had already lost them.

use serde::de::DeserializeOwned;
use std::io::{Read, Write};

const MAX_HEAD: usize = 32 * 1024;
const MAX_BODY: usize = 1024 * 1024;

#[derive(Clone, Debug, Default)]
pub struct Request {
    pub method: String,
    pub path: String,
    pub query: Vec<(String, String)>,
    pub headers: Vec<(String, String)>,
    pub body: Vec<u8>,
}

impl Request {
    pub fn header(&self, name: &str) -> Option<&str> {
        self.headers
            .iter()
            .find(|(key, _)| key.eq_ignore_ascii_case(name))
            .map(|(_, value)| value.as_str())
    }

    pub fn query(&self, name: &str) -> Option<&str> {
        self.query
            .iter()
            .find(|(key, _)| key == name)
            .map(|(_, value)| value.as_str())
    }

    pub fn json<T: DeserializeOwned>(&self) -> Result<T, String> {
        if self.body.is_empty() {
            return Err("a JSON body is required".into());
        }
        serde_json::from_slice(&self.body).map_err(|e| format!("invalid JSON body: {e}"))
    }

    /// Whether this is a WebSocket upgrade request.
    pub fn wants_upgrade(&self) -> bool {
        self.header("upgrade")
            .is_some_and(|value| value.eq_ignore_ascii_case("websocket"))
    }
}

pub struct Response {
    pub status: u16,
    pub content_type: &'static str,
    pub body: Vec<u8>,
}

impl Response {
    pub fn json(status: u16, value: serde_json::Value) -> Response {
        Response {
            status,
            content_type: "application/json",
            body: serde_json::to_vec(&value).unwrap_or_else(|_| b"{}".to_vec()),
        }
    }

    pub fn text(status: u16, text: impl Into<String>) -> Response {
        Response {
            status,
            content_type: "text/plain; charset=utf-8",
            body: text.into().into_bytes(),
        }
    }

    pub fn empty(status: u16) -> Response {
        Response {
            status,
            content_type: "text/plain; charset=utf-8",
            body: Vec::new(),
        }
    }
}

fn reason(status: u16) -> &'static str {
    match status {
        200 => "OK",
        201 => "Created",
        202 => "Accepted",
        204 => "No Content",
        400 => "Bad Request",
        401 => "Unauthorized",
        403 => "Forbidden",
        404 => "Not Found",
        405 => "Method Not Allowed",
        409 => "Conflict",
        500 => "Internal Server Error",
        503 => "Service Unavailable",
        _ => "Status",
    }
}

pub fn read_request(stream: &mut dyn Read) -> Result<Request, String> {
    let mut head = Vec::with_capacity(512);
    let mut byte = [0u8; 1];
    loop {
        match stream.read(&mut byte) {
            Ok(0) => return Err("the client closed before sending a request".into()),
            Ok(_) => head.push(byte[0]),
            Err(err) if err.kind() == std::io::ErrorKind::Interrupted => continue,
            Err(err) => return Err(format!("cannot read the request: {err}")),
        }
        if head.ends_with(b"\r\n\r\n") || head.ends_with(b"\n\n") {
            break;
        }
        if head.len() > MAX_HEAD {
            return Err("the request head is larger than this server accepts".into());
        }
    }

    let text = String::from_utf8_lossy(&head).to_string();
    let mut lines = text.split('\n').map(|line| line.trim_end_matches('\r'));
    let Some(request_line) = lines.next() else {
        return Err("the request has no request line".into());
    };
    let mut parts = request_line.split_whitespace();
    let method = parts
        .next()
        .ok_or_else(|| "the request has no method".to_owned())?
        .to_ascii_uppercase();
    let target = parts
        .next()
        .ok_or_else(|| "the request has no target".to_owned())?;
    let (path, query) = split_target(target);

    let mut headers = Vec::new();
    for line in lines {
        if line.is_empty() {
            continue;
        }
        let Some((name, value)) = line.split_once(':') else {
            return Err(format!("`{line}` is not a header"));
        };
        headers.push((name.trim().to_owned(), value.trim().to_owned()));
    }

    let length = headers
        .iter()
        .find(|(name, _)| name.eq_ignore_ascii_case("content-length"))
        .map(|(_, value)| value.parse::<usize>())
        .transpose()
        .map_err(|_| "Content-Length is not a number".to_owned())?
        .unwrap_or(0);
    if length > MAX_BODY {
        return Err("the request body is larger than this server accepts".into());
    }
    let mut body = vec![0u8; length];
    if length > 0 {
        stream
            .read_exact(&mut body)
            .map_err(|e| format!("cannot read the request body: {e}"))?;
    }

    Ok(Request {
        method,
        path,
        query,
        headers,
        body,
    })
}

fn split_target(target: &str) -> (String, Vec<(String, String)>) {
    let (path, query) = match target.split_once('?') {
        Some((path, query)) => (path, Some(query)),
        None => (target, None),
    };
    let path = path.trim_end_matches('/').to_owned();
    let path = if path.is_empty() {
        "/".to_owned()
    } else {
        path
    };
    let query = query
        .map(|query| {
            query
                .split('&')
                .filter(|pair| !pair.is_empty())
                .map(|pair| match pair.split_once('=') {
                    Some((key, value)) => (decode(key), decode(value)),
                    None => (decode(pair), String::new()),
                })
                .collect()
        })
        .unwrap_or_default();
    (path, query)
}

/// Percent-decoding for query values, which is how a WebSocket client hands over its token.
fn decode(raw: &str) -> String {
    let bytes = raw.as_bytes();
    let mut out = Vec::with_capacity(bytes.len());
    let mut index = 0;
    while index < bytes.len() {
        match bytes[index] {
            b'%' if index + 2 < bytes.len() => {
                let hex = std::str::from_utf8(&bytes[index + 1..index + 3]).unwrap_or("");
                match u8::from_str_radix(hex, 16) {
                    Ok(value) => {
                        out.push(value);
                        index += 3;
                    }
                    Err(_) => {
                        out.push(bytes[index]);
                        index += 1;
                    }
                }
            }
            b'+' => {
                out.push(b' ');
                index += 1;
            }
            byte => {
                out.push(byte);
                index += 1;
            }
        }
    }
    String::from_utf8_lossy(&out).to_string()
}

pub fn write_response(stream: &mut dyn Write, response: &Response) -> std::io::Result<()> {
    let head = format!(
        "HTTP/1.1 {} {}\r\nContent-Type: {}\r\nContent-Length: {}\r\nConnection: close\r\n\r\n",
        response.status,
        reason(response.status),
        response.content_type,
        response.body.len()
    );
    stream.write_all(head.as_bytes())?;
    stream.write_all(&response.body)?;
    stream.flush()
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::Cursor;

    #[test]
    fn a_request_with_a_query_and_a_body_is_parsed() {
        let raw = b"POST /sessions/ses_1/submit?x=1&token=vlink_ab%2Bc HTTP/1.1\r\nHost: x\r\nContent-Length: 10\r\n\r\n{\"text\":1}";
        let request = read_request(&mut Cursor::new(raw.to_vec())).unwrap();
        assert_eq!(request.method, "POST");
        assert_eq!(request.path, "/sessions/ses_1/submit");
        assert_eq!(request.query("token"), Some("vlink_ab+c"));
        assert_eq!(request.header("host"), Some("x"));
        assert_eq!(request.body, b"{\"text\":1}");
    }

    #[test]
    fn reading_stops_at_the_head_so_a_websocket_frame_survives() {
        let mut raw = b"GET /events HTTP/1.1\r\nUpgrade: websocket\r\n\r\n".to_vec();
        raw.extend_from_slice(b"\x81\x03abc");
        let mut cursor = Cursor::new(raw);
        let request = read_request(&mut cursor).unwrap();
        assert!(request.wants_upgrade());
        let mut rest = Vec::new();
        cursor.read_to_end(&mut rest).unwrap();
        assert_eq!(
            rest, b"\x81\x03abc",
            "the frame must still be on the stream"
        );
    }

    #[test]
    fn a_response_carries_its_length() {
        let mut out = Vec::new();
        write_response(&mut out, &Response::text(403, "no")).unwrap();
        let text = String::from_utf8(out).unwrap();
        assert!(text.starts_with("HTTP/1.1 403 Forbidden\r\n"));
        assert!(text.contains("Content-Length: 2\r\n"));
        assert!(text.ends_with("\r\n\r\nno"));
    }
}
