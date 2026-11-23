//! The listener: eight routes, one boundary, one audit line per action.
//!
//! One thread per connection, and the thread owns the socket from the first byte to the last. The
//! WebSocket path is the reason: tungstenite's frame codec keeps its partial frame in its own
//! buffer, so a read that times out between frames costs nothing and the same thread can drain the
//! event bus and answer a ping without a second socket handle.

use crate::audit::{Audit, AuditEntry, UNKNOWN_TOKEN};
use crate::http::{self, Request, Response};
use crate::policy::{Action, Denial, Target, authorize};
use crate::tier::{SessionOwner, Tier};
use crate::time::{iso8601, now_ms, remaining_label};
use crate::token::{
    AddRequest, Lifetime, TokenStore, Verdict, constant_time_eq, link_dir, max_temp_ms,
};
use crate::wire;
use serde::Deserialize;
use serde_json::{Value, json};
use std::collections::HashMap;
use std::io::{Read, Write};
use std::net::{SocketAddr, TcpListener, TcpStream, ToSocketAddrs};
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};
use tungstenite::protocol::{Role, WebSocket, WebSocketConfig};
use vikram_contracts::{ApprovalId, SessionId};
use vikram_runtime::RuntimeHandle;

/// How long a request may take before the connection is dropped.
const REQUEST_TIMEOUT: Duration = Duration::from_secs(30);
/// How long a WebSocket read waits before the loop drains the event bus again.
const EVENT_TICK: Duration = Duration::from_millis(25);
/// How often the session-ownership map is rebuilt on a live event stream.
const OWNER_REFRESH: Duration = Duration::from_secs(2);
/// The default audit page.
const AUDIT_PAGE: usize = 200;

#[derive(Clone, Debug)]
pub struct ServerConfig {
    /// `127.0.0.1:7717` unless the operator says otherwise.
    pub bind: String,
    pub tls_cert: Option<PathBuf>,
    pub tls_key: Option<PathBuf>,
    /// The bootstrap secret. Required for a non-loopback bind, and accepted as the owner token.
    pub env_token: Option<String>,
}

impl Default for ServerConfig {
    fn default() -> Self {
        Self {
            bind: "127.0.0.1:7717".to_owned(),
            tls_cert: None,
            tls_key: None,
            env_token: std::env::var("VIKRAM_LINK_TOKEN")
                .ok()
                .filter(|token| !token.trim().is_empty()),
        }
    }
}

struct Shared {
    handle: RuntimeHandle,
    /// The runtime's own handle. A connection thread is not inside the async runtime, and a turn is
    /// started with `tokio::spawn`, so the context is entered once per connection and every call
    /// into the runtime inherits it.
    tokio: tokio::runtime::Handle,
    tokens: Arc<Mutex<TokenStore>>,
    audit: Arc<Audit>,
    bootstrap: Option<String>,
    tls: Option<Arc<rustls::ServerConfig>>,
}

/// A token that authenticated this request.
#[derive(Clone, Debug)]
struct Auth {
    name: String,
    tier: Tier,
    owner: bool,
}

pub struct Server {
    listener: TcpListener,
    addr: SocketAddr,
    tokens: Arc<Mutex<TokenStore>>,
    audit: Arc<Audit>,
    bootstrap: Option<String>,
    tls: Option<Arc<rustls::ServerConfig>>,
}

impl Server {
    /// Bind and validate. Nothing is served until [`Server::serve`] is called, so a test can bind
    /// port 0 and learn the port before a client connects.
    pub fn bind(config: ServerConfig) -> Result<Server, String> {
        let addr = resolve(&config.bind)?;
        let bootstrap = config
            .env_token
            .map(|token| token.trim().to_owned())
            .filter(|token| !token.is_empty());
        if !addr.ip().is_loopback() && bootstrap.is_none() {
            return Err(format!(
                "refusing to bind {addr}: that address is reachable from outside this machine, so it needs a bootstrap secret — set VIKRAM_LINK_TOKEN (a long random string) or bind 127.0.0.1 and reach it through an SSH tunnel"
            ));
        }
        let tls = match (&config.tls_cert, &config.tls_key) {
            (None, None) => None,
            (Some(cert), Some(key)) => Some(Arc::new(tls_config(cert, key)?)),
            _ => {
                return Err(
                    "TLS needs both --tls-cert and --tls-key; one without the other is a mistake, not a default"
                        .into(),
                );
            }
        };
        let listener = TcpListener::bind(addr).map_err(|e| format!("cannot bind {addr}: {e}"))?;
        let addr = listener
            .local_addr()
            .map_err(|e| format!("cannot read the bound address: {e}"))?;
        let tokens = Arc::new(Mutex::new(TokenStore::at_home()?));
        let audit = Arc::new(Audit::open(link_dir().join("audit.jsonl"))?);
        Ok(Server {
            listener,
            addr,
            tokens,
            audit,
            bootstrap,
            tls,
        })
    }

    pub fn local_addr(&self) -> SocketAddr {
        self.addr
    }

    pub fn scheme(&self) -> &'static str {
        if self.tls.is_some() { "https" } else { "http" }
    }

    /// Serve until the process stops. Each connection is a thread; a slow client cannot hold up
    /// the accept loop. The runtime's handle is passed in because a turn is started with
    /// `tokio::spawn`, and these threads are not inside that runtime.
    pub fn serve(self, handle: RuntimeHandle, tokio: tokio::runtime::Handle) -> Result<(), String> {
        let shared = Arc::new(Shared {
            handle,
            tokio,
            tokens: self.tokens,
            audit: self.audit,
            bootstrap: self.bootstrap,
            tls: self.tls,
        });
        for incoming in self.listener.incoming() {
            match incoming {
                Ok(stream) => {
                    let shared = Arc::clone(&shared);
                    std::thread::spawn(move || {
                        if let Err(err) = connection(stream, shared) {
                            eprintln!("vikram link: connection failed: {err}");
                        }
                    });
                }
                Err(err) => {
                    // A failed accept is the listener's problem, not a client's: report it and keep
                    // serving, because one transient error must not take every linked device down.
                    eprintln!("vikram link: accept failed: {err}");
                }
            }
        }
        Ok(())
    }
}

fn resolve(bind: &str) -> Result<SocketAddr, String> {
    bind.to_socket_addrs()
        .map_err(|e| format!("`{bind}` is not an address this machine can bind: {e}"))?
        .next()
        .ok_or_else(|| format!("`{bind}` did not resolve to an address"))
}

/// A connection is either a plain socket or the same socket under TLS. The variants exist so the
/// read timeout can still be set on the underlying `TcpStream` after the TLS layer is wrapped
/// around it, which is what lets the event loop poll without a second thread.
enum Conn {
    Plain(TcpStream),
    Tls(Box<rustls::StreamOwned<rustls::ServerConnection, TcpStream>>),
}

impl Conn {
    fn socket(&self) -> &TcpStream {
        match self {
            Conn::Plain(stream) => stream,
            Conn::Tls(stream) => &stream.sock,
        }
    }

    fn set_read_timeout(&self, timeout: Option<Duration>) {
        let _ = self.socket().set_read_timeout(timeout);
    }
}

impl Read for Conn {
    fn read(&mut self, buf: &mut [u8]) -> std::io::Result<usize> {
        match self {
            Conn::Plain(stream) => stream.read(buf),
            Conn::Tls(stream) => stream.read(buf),
        }
    }
}

impl Write for Conn {
    fn write(&mut self, buf: &[u8]) -> std::io::Result<usize> {
        match self {
            Conn::Plain(stream) => stream.write(buf),
            Conn::Tls(stream) => stream.write(buf),
        }
    }

    fn flush(&mut self) -> std::io::Result<()> {
        match self {
            Conn::Plain(stream) => stream.flush(),
            Conn::Tls(stream) => stream.flush(),
        }
    }
}

fn connection(stream: TcpStream, shared: Arc<Shared>) -> Result<(), String> {
    let _entered = shared.tokio.enter();
    let _ = stream.set_nodelay(true);
    let _ = stream.set_read_timeout(Some(REQUEST_TIMEOUT));
    let _ = stream.set_write_timeout(Some(REQUEST_TIMEOUT));
    let mut conn = match &shared.tls {
        Some(config) => {
            let session = rustls::ServerConnection::new(Arc::clone(config))
                .map_err(|e| format!("cannot start TLS: {e}"))?;
            Conn::Tls(Box::new(rustls::StreamOwned::new(session, stream)))
        }
        None => Conn::Plain(stream),
    };
    let request = match http::read_request(&mut conn) {
        Ok(request) => request,
        Err(err) => {
            let _ = http::write_response(&mut conn, &Response::json(400, json!({"error": err})));
            return Ok(());
        }
    };
    if request.path == "/events" {
        return events(conn, &request, &shared);
    }
    let response = route(&request, &shared);
    http::write_response(&mut conn, &response).map_err(|e| format!("cannot answer: {e}"))
}

fn route(request: &Request, shared: &Shared) -> Response {
    let segments: Vec<&str> = request
        .path
        .trim_start_matches('/')
        .split('/')
        .filter(|part| !part.is_empty())
        .collect();
    // Liveness is deliberately open: a probe that needs a secret is a probe that pages somebody at
    // three in the morning. It answers one bit and a version, nothing about this runtime.
    if request.method == "GET" && segments == ["health"] {
        return Response::json(
            200,
            json!({
                "ok": true,
                "service": "vikram-link",
                "version": env!("CARGO_PKG_VERSION"),
            }),
        );
    }
    let auth = match authenticate(request, shared) {
        Ok(auth) => auth,
        Err(response) => return response,
    };
    match (request.method.as_str(), segments.as_slice()) {
        ("GET", ["sessions"]) => guarded(
            shared,
            &auth,
            Action::ListSessions,
            None,
            "sessions",
            || list_sessions(&auth, shared),
        ),
        ("POST", ["sessions", id, "submit"]) => submit(shared, &auth, id, request),
        ("GET", ["link", "tokens"]) => {
            guarded(shared, &auth, Action::ListTokens, None, "tokens", || {
                list_tokens(shared)
            })
        }
        ("POST", ["link", "tokens"]) => create_token(shared, &auth, request),
        ("DELETE", ["link", "tokens", name]) => {
            guarded(shared, &auth, Action::ManageTokens, None, name, || {
                revoke_token(shared, name)
            })
        }
        ("GET", ["link", "audit"]) => {
            guarded(shared, &auth, Action::ReadAudit, None, "audit", || {
                read_audit(shared, request)
            })
        }
        ("GET", ["link", "approvals"]) => guarded(
            shared,
            &auth,
            Action::ListApprovals,
            None,
            "approvals",
            || list_approvals(shared),
        ),
        ("POST", ["link", "approvals", id]) => answer_approval(shared, &auth, id, request),
        ("GET", ["events"]) => Response::json(
            400,
            json!({"error": "`/events` is a WebSocket endpoint; send an Upgrade request"}),
        ),
        _ => Response::json(
            404,
            json!({"error": format!("no route for {} {}", request.method, request.path)}),
        ),
    }
}

/// The single gate every route goes through: authorize, run, and write one audit line whatever
/// happened. A denied action is recorded as well as a successful one — that is the point of it.
fn guarded(
    shared: &Shared,
    auth: &Auth,
    action: Action,
    target: Option<&Target>,
    label: &str,
    run: impl FnOnce() -> Response,
) -> Response {
    if let Err(denial) = authorize(&auth.tier, auth.owner, action, target) {
        let reason = denial.to_string();
        let entry = AuditEntry::ok(
            now_ms(),
            &auth.name,
            auth.tier.to_string(),
            action.as_str(),
            label,
        )
        .denied(&reason);
        let _ = shared.audit.record(&entry);
        return refusal(&denial);
    }
    let response = run();
    let entry = AuditEntry::ok(
        now_ms(),
        &auth.name,
        auth.tier.to_string(),
        action.as_str(),
        label,
    );
    let entry = if response.status >= 400 {
        entry.failed(format!("status {}", response.status))
    } else {
        entry
    };
    match shared.audit.record(&entry) {
        Ok(()) => response,
        Err(err) => Response::json(
            500,
            json!({"error": format!("the audit log could not be written: {err}")}),
        ),
    }
}

fn refusal(denial: &Denial) -> Response {
    match denial {
        Denial::Unauthorized(reason) => Response::json(401, json!({"error": reason})),
        Denial::Forbidden { tier, reason } => {
            Response::json(403, json!({"error": reason, "tier": tier}))
        }
    }
}

fn authenticate(request: &Request, shared: &Shared) -> Result<Auth, Response> {
    let bearer = request
        .header("authorization")
        .and_then(|value| {
            value
                .strip_prefix("Bearer ")
                .or_else(|| value.strip_prefix("bearer "))
        })
        .map(str::trim)
        .filter(|value| !value.is_empty());
    let presented = bearer
        .or_else(|| request.header("x-vikram-token").map(str::trim))
        .or_else(|| request.query("token"))
        .filter(|value| !value.is_empty());
    let Some(secret) = presented else {
        let _ = shared.audit.record(
            &AuditEntry::ok(now_ms(), UNKNOWN_TOKEN, "-", "auth", &request.path)
                .denied("no token presented"),
        );
        return Err(Response::json(
            401,
            json!({"error": "missing token: send `Authorization: Bearer <secret>`"}),
        ));
    };

    if let Some(bootstrap) = &shared.bootstrap
        && constant_time_eq(bootstrap.as_bytes(), secret.as_bytes())
    {
        return Ok(Auth {
            name: "env:VIKRAM_LINK_TOKEN".to_owned(),
            tier: Tier::Admin,
            owner: true,
        });
    }

    let outcome = {
        let mut store = match shared.tokens.lock() {
            Ok(store) => store,
            Err(_) => {
                return Err(Response::json(
                    500,
                    json!({"error": "the token store is locked"}),
                ));
            }
        };
        store.verify(secret, now_ms())
    };
    match outcome {
        Verdict::Valid(verified) => Ok(Auth {
            name: verified.name,
            tier: verified.tier,
            owner: verified.owner,
        }),
        Verdict::Revoked { name } => {
            let _ = shared.audit.record(
                &AuditEntry::ok(now_ms(), &name, "-", "auth", &request.path).denied("revoked"),
            );
            Err(Response::json(
                401,
                json!({"error": format!("token `{name}` was revoked")}),
            ))
        }
        Verdict::Expired { name, expires_ms } => {
            let _ = shared.audit.record(
                &AuditEntry::ok(now_ms(), &name, "-", "auth", &request.path).denied("expired"),
            );
            Err(Response::json(
                401,
                json!({
                    "error": format!("token `{name}` expired at {}", iso8601(expires_ms)),
                    "expired_at": iso8601(expires_ms),
                }),
            ))
        }
        Verdict::Unknown => {
            let _ = shared.audit.record(
                &AuditEntry::ok(now_ms(), UNKNOWN_TOKEN, "-", "auth", &request.path)
                    .denied("unknown token"),
            );
            Err(Response::json(401, json!({"error": "unknown token"})))
        }
    }
}

/// Who owns each session: a bot's own conversation, a team's group conversation, or the operator's
/// root session. This is what every reach decision is made against.
fn session_owners(shared: &Shared) -> HashMap<String, SessionOwner> {
    let snapshot = shared.handle.snapshot();
    let bots: HashMap<String, String> = snapshot
        .bots
        .iter()
        .map(|bot| (bot.profile.id.to_string(), bot.profile.name.clone()))
        .collect();
    let mut map: HashMap<String, SessionOwner> = snapshot
        .sessions
        .iter()
        .map(|session| {
            let owner = match &session.bot {
                Some(id) => SessionOwner::Bot(
                    bots.get(&id.to_string())
                        .cloned()
                        .unwrap_or_else(|| id.to_string()),
                ),
                None => SessionOwner::Root,
            };
            (session.id.to_string(), owner)
        })
        .collect();
    if let Ok(hives) = shared.handle.hive_list() {
        for hive in hives {
            if let Some(session) = hive.session_id {
                map.insert(session.to_string(), SessionOwner::Team(hive.name));
            }
        }
    }
    map
}

fn owner_of(map: &HashMap<String, SessionOwner>, id: &str) -> SessionOwner {
    map.get(id).cloned().unwrap_or(SessionOwner::Root)
}

fn session_row(session: &vikram_runtime::types::SessionSnapshot, owner: &SessionOwner) -> Value {
    json!({
        "id": session.id.to_string(),
        "title": session.title,
        "model": session.model,
        "kind": owner.kind(),
        "entity": owner.name(),
        "status": session.status,
        "messages": session.messages.len(),
        "queued": session.queued,
        "updated_ms": session.updated_ms,
    })
}

fn list_sessions(auth: &Auth, shared: &Shared) -> Response {
    let snapshot = shared.handle.snapshot();
    let owners = session_owners(shared);
    let rows: Vec<Value> = snapshot
        .sessions
        .iter()
        .filter_map(|session| {
            let owner = owner_of(&owners, session.id.as_str());
            auth.tier
                .reaches(&owner)
                .then(|| session_row(session, &owner))
        })
        .collect();
    Response::json(
        200,
        json!({"tier": auth.tier.to_string(), "sessions": rows}),
    )
}

#[derive(Deserialize)]
struct SubmitBody {
    text: String,
}

fn submit(shared: &Shared, auth: &Auth, id: &str, request: &Request) -> Response {
    let snapshot = shared.handle.snapshot();
    let Some(session) = snapshot.sessions.iter().find(|s| s.id.as_str() == id) else {
        return Response::json(404, json!({"error": format!("no session `{id}`")}));
    };
    let owner = owner_of(&session_owners(shared), session.id.as_str());
    let target = Target {
        id: id.to_owned(),
        owner,
    };
    guarded(shared, auth, Action::Submit, Some(&target), id, || {
        let body: SubmitBody = match request.json() {
            Ok(body) => body,
            Err(err) => {
                return Response::json(
                    400,
                    json!({"error": err, "expected": {"text": "the turn to run"}}),
                );
            }
        };
        if body.text.trim().is_empty() {
            return Response::json(400, json!({"error": "`text` cannot be empty"}));
        }
        match shared.handle.submit(&SessionId::new(id), body.text) {
            Ok(()) => Response::json(202, json!({"accepted": true, "session": id})),
            Err(err) => Response::json(409, json!({"error": err.to_string()})),
        }
    })
}

fn token_row(summary: crate::token::TokenSummary, now: u64) -> Value {
    json!({
        "name": summary.name,
        "tier": summary.tier.to_string(),
        "created": iso8601(summary.created_ms),
        "expires": summary.expires_ms.map(iso8601),
        "expires_in": summary.expires_ms.map(|expires| remaining_label(now, expires)),
        "permanent": summary.permanent(),
        "last_seen": summary.last_seen_ms.map(iso8601),
        "revoked": summary.revoked,
        "owner": summary.owner,
        "state": summary.state(now),
    })
}

fn list_tokens(shared: &Shared) -> Response {
    let now = now_ms();
    let rows = match shared.tokens.lock() {
        Ok(store) => store
            .summaries(now)
            .into_iter()
            .map(|summary| token_row(summary, now))
            .collect::<Vec<_>>(),
        Err(_) => {
            return Response::json(500, json!({"error": "the token store is locked"}));
        }
    };
    Response::json(200, json!({"tokens": rows}))
}

#[derive(Deserialize)]
struct CreateToken {
    name: String,
    tier: String,
    /// `--for`: a duration such as `30m`.
    #[serde(rename = "for")]
    span: Option<String>,
    #[serde(default)]
    permanent: bool,
    #[serde(default)]
    owner: bool,
}

fn create_token(shared: &Shared, auth: &Auth, request: &Request) -> Response {
    guarded(shared, auth, Action::ManageTokens, None, "tokens", || {
        let body: CreateToken = match request.json() {
            Ok(body) => body,
            Err(err) => return Response::json(400, json!({"error": err})),
        };
        let lifetime = match (body.span, body.permanent) {
            (Some(span), false) => Lifetime::For(span),
            (None, true) => Lifetime::Permanent,
            (Some(_), true) => {
                return Response::json(
                    400,
                    json!({"error": "give `for` or `permanent`, not both"}),
                );
            }
            (None, false) => {
                return Response::json(
                    400,
                    json!({"error": "give `for` (a duration such as `30m`) or `permanent`"}),
                );
            }
        };
        let ceiling = match max_temp_ms() {
            Ok(ceiling) => ceiling,
            Err(err) => return Response::json(400, json!({"error": err})),
        };
        let mut store = match shared.tokens.lock() {
            Ok(store) => store,
            Err(_) => {
                return Response::json(500, json!({"error": "the token store is locked"}));
            }
        };
        let minted = match store.add(
            AddRequest {
                name: body.name,
                tier: body.tier,
                lifetime,
                owner: body.owner,
            },
            now_ms(),
            ceiling,
        ) {
            Ok(minted) => minted,
            Err(err) => return Response::json(400, json!({"error": err})),
        };
        Response::json(
            201,
            json!({
                "name": minted.name,
                "tier": minted.tier.to_string(),
                "secret": minted.secret,
                "expires": minted.expires_ms.map(iso8601),
            }),
        )
    })
}

fn revoke_token(shared: &Shared, name: &str) -> Response {
    let mut store = match shared.tokens.lock() {
        Ok(store) => store,
        Err(_) => return Response::json(500, json!({"error": "the token store is locked"})),
    };
    match store.revoke(name) {
        Ok(true) => Response::json(200, json!({"revoked": name})),
        Ok(false) => Response::json(404, json!({"error": format!("no token `{name}`")})),
        Err(err) => Response::json(500, json!({"error": err})),
    }
}

fn read_audit(shared: &Shared, request: &Request) -> Response {
    let limit = request
        .query("limit")
        .and_then(|raw| raw.parse::<usize>().ok())
        .unwrap_or(AUDIT_PAGE)
        .clamp(1, 2_000);
    match shared.audit.tail(limit) {
        Ok(records) => Response::json(200, json!({"records": records})),
        Err(err) => Response::json(500, json!({"error": err})),
    }
}

fn list_approvals(shared: &Shared) -> Response {
    let snapshot = shared.handle.snapshot();
    let rows: Vec<Value> = snapshot.approvals.iter().map(wire::approval).collect();
    Response::json(200, json!({"approvals": rows}))
}

#[derive(Deserialize)]
struct AnswerBody {
    allow: bool,
    #[serde(default)]
    persist: bool,
}

fn answer_approval(shared: &Shared, auth: &Auth, id: &str, request: &Request) -> Response {
    let snapshot = shared.handle.snapshot();
    let Some(prompt) = snapshot
        .approvals
        .iter()
        .find(|prompt| prompt.approval.as_str() == id)
    else {
        return Response::json(
            404,
            json!({"error": format!("no approval `{id}` is pending")}),
        );
    };
    let target = Target {
        id: prompt.session.to_string(),
        owner: owner_of(&session_owners(shared), prompt.session.as_str()),
    };
    let body: AnswerBody = match request.json() {
        Ok(body) => body,
        Err(err) => return Response::json(400, json!({"error": err})),
    };
    guarded(
        shared,
        auth,
        Action::AnswerApproval {
            persist: body.persist,
        },
        Some(&target),
        id,
        || match shared
            .handle
            .answer_approval(&ApprovalId::new(id), body.allow, body.persist)
        {
            Ok(()) => Response::json(
                200,
                json!({"approval": id, "allowed": body.allow, "persisted": body.persist}),
            ),
            Err(err) => Response::json(409, json!({"error": err.to_string()})),
        },
    )
}

fn events(mut conn: Conn, request: &Request, shared: &Shared) -> Result<(), String> {
    let auth = match authenticate(request, shared) {
        Ok(auth) => auth,
        Err(response) => {
            let _ = http::write_response(&mut conn, &response);
            return Ok(());
        }
    };
    if let Err(denial) = authorize(&auth.tier, auth.owner, Action::ReadEvents, None) {
        let _ = shared.audit.record(
            &AuditEntry::ok(
                now_ms(),
                &auth.name,
                auth.tier.to_string(),
                Action::ReadEvents.as_str(),
                "events",
            )
            .denied(denial.to_string()),
        );
        let _ = http::write_response(&mut conn, &refusal(&denial));
        return Ok(());
    }
    if !request.wants_upgrade() {
        let response = Response::json(400, json!({"error": "`/events` needs a WebSocket upgrade"}));
        let _ = http::write_response(&mut conn, &response);
        return Ok(());
    }
    let Some(key) = request.header("sec-websocket-key") else {
        let response = Response::json(400, json!({"error": "missing Sec-WebSocket-Key"}));
        let _ = http::write_response(&mut conn, &response);
        return Ok(());
    };
    let version = request.header("sec-websocket-version").unwrap_or_default();
    if version != "13" {
        let response = Response::json(400, json!({"error": "Sec-WebSocket-Version must be 13"}));
        let _ = http::write_response(&mut conn, &response);
        return Ok(());
    }

    let accept = tungstenite::handshake::derive_accept_key(key.as_bytes());
    let head = format!(
        "HTTP/1.1 101 Switching Protocols\r\nConnection: Upgrade\r\nUpgrade: websocket\r\nSec-WebSocket-Accept: {accept}\r\n\r\n"
    );
    conn.write_all(head.as_bytes())
        .and_then(|()| conn.flush())
        .map_err(|e| format!("cannot complete the WebSocket handshake: {e}"))?;

    shared
        .audit
        .record(&AuditEntry::ok(
            now_ms(),
            &auth.name,
            auth.tier.to_string(),
            Action::ReadEvents.as_str(),
            "events",
        ))
        .map_err(|err| format!("the audit log could not be written: {err}"))?;

    conn.set_read_timeout(Some(EVENT_TICK));
    let mut socket =
        WebSocket::from_raw_socket(conn, Role::Server, Some(WebSocketConfig::default()));
    let mut bus = shared.handle.events();
    let mut owners = session_owners(shared);
    let mut refreshed = Instant::now();
    loop {
        loop {
            match bus.try_recv() {
                Ok(event) => {
                    if let Some(session) = wire::event_session(&event) {
                        let owner = owner_of(&owners, session.as_str());
                        if !auth.tier.reaches(&owner) {
                            continue;
                        }
                    }
                    let payload = wire::event(&event).to_string();
                    if socket
                        .send(tungstenite::Message::Text(payload.into()))
                        .is_err()
                    {
                        return Ok(());
                    }
                }
                Err(tokio::sync::broadcast::error::TryRecvError::Empty) => break,
                Err(tokio::sync::broadcast::error::TryRecvError::Lagged(missed)) => {
                    let payload = json!({"type": "lagged", "missed": missed}).to_string();
                    if socket
                        .send(tungstenite::Message::Text(payload.into()))
                        .is_err()
                    {
                        return Ok(());
                    }
                }
                Err(tokio::sync::broadcast::error::TryRecvError::Closed) => return Ok(()),
            }
        }
        match socket.read() {
            Ok(tungstenite::Message::Close(_)) => {
                let _ = socket.close(None);
                return Ok(());
            }
            Ok(_) => {}
            Err(tungstenite::Error::Io(err))
                if matches!(
                    err.kind(),
                    std::io::ErrorKind::WouldBlock | std::io::ErrorKind::TimedOut
                ) => {}
            Err(tungstenite::Error::ConnectionClosed) | Err(tungstenite::Error::AlreadyClosed) => {
                return Ok(());
            }
            Err(_) => return Ok(()),
        }
        if refreshed.elapsed() >= OWNER_REFRESH {
            owners = session_owners(shared);
            refreshed = Instant::now();
        }
    }
}

fn tls_config(cert: &Path, key_path: &Path) -> Result<rustls::ServerConfig, String> {
    use rustls::pki_types::pem::PemObject;
    use rustls::pki_types::{
        CertificateDer, PrivateKeyDer, PrivatePkcs1KeyDer, PrivatePkcs8KeyDer, PrivateSec1KeyDer,
    };
    let _ = rustls::crypto::ring::default_provider().install_default();
    let certs: Vec<CertificateDer<'static>> = CertificateDer::pem_file_iter(cert)
        .map_err(|e| format!("cannot read {}: {e}", cert.display()))?
        .collect::<Result<Vec<_>, _>>()
        .map_err(|e| format!("{} is not a PEM certificate: {e}", cert.display()))?;
    if certs.is_empty() {
        return Err(format!("{} holds no certificate", cert.display()));
    }
    let bytes =
        std::fs::read(key_path).map_err(|e| format!("cannot read {}: {e}", key_path.display()))?;
    let key = PrivatePkcs8KeyDer::from_pem_slice(&bytes)
        .map(PrivateKeyDer::Pkcs8)
        .or_else(|_| PrivateSec1KeyDer::from_pem_slice(&bytes).map(PrivateKeyDer::Sec1))
        .or_else(|_| PrivatePkcs1KeyDer::from_pem_slice(&bytes).map(PrivateKeyDer::Pkcs1))
        .map_err(|_| {
            format!(
                "{} is not a PEM private key (PKCS#8, SEC1 or PKCS#1)",
                key_path.display()
            )
        })?;
    rustls::ServerConfig::builder()
        .with_no_client_auth()
        .with_single_cert(certs, key)
        .map_err(|e| format!("the certificate and key do not match: {e}"))
}
