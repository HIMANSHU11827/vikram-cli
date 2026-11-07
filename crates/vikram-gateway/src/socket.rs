//! A line-oriented socket seam for platforms that are not HTTP.
//!
//! IRC, mail and local bridges speak a line protocol over a stream. Adapters take a [`LineSocket`]
//! so their protocol logic — what to send, how to parse a line, which error kind a refusal maps to
//! — is testable with a scripted mock, exactly like the HTTP adapters use
//! [`crate::transport::Transport`].
//!
//! Three implementations exist: [`TcpLineSocket`] (plain), [`TlsLineSocket`] (rustls with the
//! webpki roots), and [`WsSocket`] (WebSocket text frames, `ws://` or `wss://`).

use async_trait::async_trait;
use std::sync::Arc;
use tokio::io::{AsyncBufReadExt, AsyncWriteExt, BufReader};
use tokio::net::TcpStream;
use tokio::sync::Mutex;
use tokio_rustls::TlsConnector;
use tokio_rustls::rustls::pki_types::ServerName;
use tokio_rustls::rustls::{ClientConfig, RootCertStore};

#[derive(Debug, thiserror::Error)]
pub enum SocketError {
    #[error("socket io error: {0}")]
    Io(String),
    #[error("socket is closed")]
    Closed,
}

#[async_trait]
pub trait LineSocket: Send + Sync {
    async fn send_line(&self, line: &str) -> Result<(), SocketError>;
    /// Next line, or `None` at end of stream.
    async fn next_line(&self) -> Result<Option<String>, SocketError>;
}

pub type SharedLineSocket = Arc<dyn LineSocket>;

/// A WebSocket connection presented as lines. Text frames are lines; binary frames that happen to
/// be valid UTF-8 are forwarded too (several local bridges send JSON that way).
///
/// Both `ws://` and `wss://` are supported: the crate enables `tungstenite`'s rustls backend. The
/// read timeout is applied to the TCP stream underneath either variant, so an idle socket never
/// blocks outbound writes.
pub struct WsSocket {
    outbound: tokio::sync::mpsc::Sender<String>,
    inbound: Mutex<tokio::sync::mpsc::Receiver<String>>,
}

impl WsSocket {
    pub fn connect(url: &str) -> Result<Self, SocketError> {
        let (out_tx, mut out_rx) = tokio::sync::mpsc::channel::<String>(64);
        let (in_tx, in_rx) = tokio::sync::mpsc::channel::<String>(64);
        let (ready_tx, ready_rx) = std::sync::mpsc::channel::<Result<(), String>>();
        let target = url.to_string();

        std::thread::Builder::new()
            .name("vikram-ws".to_string())
            .spawn(move || {
                let (mut ws, _) = match tungstenite::connect(target.as_str()) {
                    Ok(pair) => {
                        let _ = ready_tx.send(Ok(()));
                        pair
                    }
                    Err(err) => {
                        let _ = ready_tx.send(Err(err.to_string()));
                        return;
                    }
                };
                match ws.get_mut() {
                    tungstenite::stream::MaybeTlsStream::Plain(tcp) => {
                        let _ = tcp.set_read_timeout(Some(std::time::Duration::from_millis(50)));
                    }
                    tungstenite::stream::MaybeTlsStream::Rustls(stream) => {
                        let _ = stream
                            .sock
                            .set_read_timeout(Some(std::time::Duration::from_millis(50)));
                    }
                    _ => {}
                }
                loop {
                    while let Ok(line) = out_rx.try_recv() {
                        if ws.send(tungstenite::Message::Text(line.into())).is_err() {
                            return;
                        }
                    }
                    match ws.read() {
                        Ok(tungstenite::Message::Text(text)) => {
                            if in_tx.blocking_send(text.to_string()).is_err() {
                                return;
                            }
                        }
                        Ok(tungstenite::Message::Binary(bytes)) => {
                            if let Ok(text) = String::from_utf8(bytes.to_vec())
                                && in_tx.blocking_send(text).is_err()
                            {
                                return;
                            }
                        }
                        Ok(tungstenite::Message::Close(_)) => return,
                        Ok(_) => {}
                        Err(tungstenite::Error::Io(err))
                            if err.kind() == std::io::ErrorKind::WouldBlock
                                || err.kind() == std::io::ErrorKind::TimedOut => {}
                        Err(_) => return,
                    }
                }
            })
            .map_err(|err| SocketError::Io(err.to_string()))?;

        match ready_rx.recv_timeout(std::time::Duration::from_secs(15)) {
            Ok(Ok(())) => {}
            Ok(Err(err)) => return Err(SocketError::Io(err)),
            Err(_) => return Err(SocketError::Io("websocket connect timed out".into())),
        }

        Ok(Self {
            outbound: out_tx,
            inbound: Mutex::new(in_rx),
        })
    }

    pub fn into_shared(self) -> SharedLineSocket {
        Arc::new(self)
    }
}

#[async_trait]
impl LineSocket for WsSocket {
    async fn send_line(&self, line: &str) -> Result<(), SocketError> {
        self.outbound
            .send(line.to_string())
            .await
            .map_err(|_| SocketError::Closed)
    }

    async fn next_line(&self) -> Result<Option<String>, SocketError> {
        let mut inbound = self.inbound.lock().await;
        Ok(inbound.recv().await)
    }
}

/// A real TCP line socket. Writes are serialized so two callers cannot interleave halves of a
/// line; reads are serialized for the same reason.
pub struct TcpLineSocket {
    writer: Mutex<tokio::net::tcp::OwnedWriteHalf>,
    reader: Mutex<BufReader<tokio::net::tcp::OwnedReadHalf>>,
}

impl TcpLineSocket {
    pub async fn connect(host: &str, port: u16) -> Result<Self, SocketError> {
        let stream = TcpStream::connect((host, port))
            .await
            .map_err(|err| SocketError::Io(err.to_string()))?;
        let (read, write) = stream.into_split();
        Ok(Self {
            writer: Mutex::new(write),
            reader: Mutex::new(BufReader::new(read)),
        })
    }

    pub fn into_shared(self) -> SharedLineSocket {
        Arc::new(self)
    }
}

#[async_trait]
impl LineSocket for TcpLineSocket {
    async fn send_line(&self, line: &str) -> Result<(), SocketError> {
        let mut writer = self.writer.lock().await;
        writer
            .write_all(line.as_bytes())
            .await
            .map_err(|err| SocketError::Io(err.to_string()))?;
        writer
            .write_all(b"\r\n")
            .await
            .map_err(|err| SocketError::Io(err.to_string()))?;
        writer
            .flush()
            .await
            .map_err(|err| SocketError::Io(err.to_string()))
    }

    async fn next_line(&self) -> Result<Option<String>, SocketError> {
        let mut reader = self.reader.lock().await;
        let mut line = String::new();
        match reader.read_line(&mut line).await {
            Ok(0) => Ok(None),
            Ok(_) => Ok(Some(line.trim_end_matches(['\r', '\n']).to_string())),
            Err(err) => Err(SocketError::Io(err.to_string())),
        }
    }
}

/// A TLS line socket (rustls, webpki roots). Used by mail and by any `ircs://` connection the host
/// chooses to make safe on its own; the adapter still only sees lines.
pub struct TlsLineSocket {
    writer: Mutex<tokio::io::WriteHalf<tokio_rustls::client::TlsStream<TcpStream>>>,
    reader: Mutex<BufReader<tokio::io::ReadHalf<tokio_rustls::client::TlsStream<TcpStream>>>>,
}

impl TlsLineSocket {
    pub async fn connect(host: &str, port: u16) -> Result<Self, SocketError> {
        let mut roots = RootCertStore::empty();
        roots.extend(webpki_roots::TLS_SERVER_ROOTS.iter().cloned());
        let config = ClientConfig::builder()
            .with_root_certificates(roots)
            .with_no_client_auth();
        let connector = TlsConnector::from(Arc::new(config));
        let tcp = TcpStream::connect((host, port))
            .await
            .map_err(|err| SocketError::Io(err.to_string()))?;
        let server_name = ServerName::try_from(host.to_owned())
            .map_err(|err| SocketError::Io(format!("invalid TLS server name: {err}")))?;
        let stream = connector
            .connect(server_name, tcp)
            .await
            .map_err(|err| SocketError::Io(format!("TLS handshake failed: {err}")))?;
        let (read, write) = tokio::io::split(stream);
        Ok(Self {
            writer: Mutex::new(write),
            reader: Mutex::new(BufReader::new(read)),
        })
    }

    pub fn into_shared(self) -> SharedLineSocket {
        Arc::new(self)
    }
}

#[async_trait]
impl LineSocket for TlsLineSocket {
    async fn send_line(&self, line: &str) -> Result<(), SocketError> {
        let mut writer = self.writer.lock().await;
        writer
            .write_all(line.as_bytes())
            .await
            .map_err(|err| SocketError::Io(err.to_string()))?;
        writer
            .write_all(b"\r\n")
            .await
            .map_err(|err| SocketError::Io(err.to_string()))?;
        writer
            .flush()
            .await
            .map_err(|err| SocketError::Io(err.to_string()))
    }

    async fn next_line(&self) -> Result<Option<String>, SocketError> {
        let mut reader = self.reader.lock().await;
        let mut line = String::new();
        match reader.read_line(&mut line).await {
            Ok(0) => Ok(None),
            Ok(_) => Ok(Some(line.trim_end_matches(['\r', '\n']).to_string())),
            Err(err) => Err(SocketError::Io(err.to_string())),
        }
    }
}

/// A framed byte socket, for protocols whose frames are protobuf or other binary payloads
/// (Yuanbao). Text frames are surfaced as their UTF-8 bytes so a mixed server does not stall.
#[async_trait]
pub trait BinarySocket: Send + Sync {
    async fn send_binary(&self, data: &[u8]) -> Result<(), SocketError>;
    /// Next frame, or `None` at end of stream.
    async fn next_binary(&self) -> Result<Option<Vec<u8>>, SocketError>;
}

/// WebSocket carrying binary frames. Same dedicated-thread model as [`WsSocket`]: the socket is
/// owned by one thread, reads time out every 50 ms so a pending write never waits on a read.
pub struct WsBinarySocket {
    outbound: tokio::sync::mpsc::Sender<Vec<u8>>,
    inbound: Mutex<tokio::sync::mpsc::Receiver<Vec<u8>>>,
}

impl WsBinarySocket {
    pub fn connect(url: &str) -> Result<Self, SocketError> {
        let (out_tx, mut out_rx) = tokio::sync::mpsc::channel::<Vec<u8>>(64);
        let (in_tx, in_rx) = tokio::sync::mpsc::channel::<Vec<u8>>(64);
        let (ready_tx, ready_rx) = std::sync::mpsc::channel::<Result<(), String>>();
        let target = url.to_string();

        std::thread::Builder::new()
            .name("vikram-ws-binary".to_string())
            .spawn(move || {
                let (mut ws, _) = match tungstenite::connect(target.as_str()) {
                    Ok(pair) => {
                        let _ = ready_tx.send(Ok(()));
                        pair
                    }
                    Err(err) => {
                        let _ = ready_tx.send(Err(err.to_string()));
                        return;
                    }
                };
                match ws.get_mut() {
                    tungstenite::stream::MaybeTlsStream::Plain(tcp) => {
                        let _ = tcp.set_read_timeout(Some(std::time::Duration::from_millis(50)));
                    }
                    tungstenite::stream::MaybeTlsStream::Rustls(stream) => {
                        let _ = stream
                            .sock
                            .set_read_timeout(Some(std::time::Duration::from_millis(50)));
                    }
                    _ => {}
                }
                loop {
                    while let Ok(frame) = out_rx.try_recv() {
                        if ws.send(tungstenite::Message::Binary(frame.into())).is_err() {
                            return;
                        }
                    }
                    match ws.read() {
                        Ok(tungstenite::Message::Binary(bytes)) => {
                            if in_tx.blocking_send(bytes.to_vec()).is_err() {
                                return;
                            }
                        }
                        Ok(tungstenite::Message::Text(text)) => {
                            if in_tx.blocking_send(text.to_string().into_bytes()).is_err() {
                                return;
                            }
                        }
                        Ok(tungstenite::Message::Close(_)) => return,
                        Ok(_) => {}
                        Err(tungstenite::Error::Io(err))
                            if err.kind() == std::io::ErrorKind::WouldBlock
                                || err.kind() == std::io::ErrorKind::TimedOut => {}
                        Err(_) => return,
                    }
                }
            })
            .map_err(|err| SocketError::Io(err.to_string()))?;

        match ready_rx.recv_timeout(std::time::Duration::from_secs(15)) {
            Ok(Ok(())) => {}
            Ok(Err(err)) => return Err(SocketError::Io(err)),
            Err(_) => return Err(SocketError::Io("websocket connect timed out".into())),
        }

        Ok(Self {
            outbound: out_tx,
            inbound: Mutex::new(in_rx),
        })
    }
}

#[async_trait]
impl BinarySocket for WsBinarySocket {
    async fn send_binary(&self, data: &[u8]) -> Result<(), SocketError> {
        self.outbound
            .send(data.to_vec())
            .await
            .map_err(|_| SocketError::Closed)
    }

    async fn next_binary(&self) -> Result<Option<Vec<u8>>, SocketError> {
        let mut inbound = self.inbound.lock().await;
        Ok(inbound.recv().await)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn errors_are_distinguishable() {
        assert!(matches!(SocketError::Closed, SocketError::Closed));
        assert_eq!(
            SocketError::Io("x".into()).to_string(),
            "socket io error: x"
        );
    }
}
