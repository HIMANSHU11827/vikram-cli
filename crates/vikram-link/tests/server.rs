// The process-global home lock is held for the whole of each test: it is what keeps two tests from
// sharing one `VIKRAM_HOME`, and the guard is not used to hand anything between threads.
#![allow(clippy::await_holding_lock)]

use serde_json::{Value, json};
use std::io::{Read, Write};
use std::net::{SocketAddr, TcpStream};
use std::path::Path;
use std::sync::MutexGuard;
use std::time::Duration;
use vikram_contracts::{SessionId, UserId};
use vikram_link::token::link_dir;
use vikram_link::{AddRequest, Lifetime, Server, ServerConfig, TokenStore};
use vikram_runtime::{Config, Runtime, RuntimeHandle};

fn isolated_home(dir: &Path) -> MutexGuard<'static, ()> {
    let guard = vikram_runtime::config::lock_security_env();
    unsafe { std::env::set_var("VIKRAM_HOME", dir.join("home")) };
    guard
}

fn config_for(dir: &Path) -> Config {
    Config {
        model: "test-model".into(),
        workspace_root: Some(dir.to_path_buf()),
        ..Config::default()
    }
}

struct Harness {
    _dir: tempfile::TempDir,
    _guard: MutexGuard<'static, ()>,
    tokio: tokio::runtime::Runtime,
    runtime: Runtime,
    handle: RuntimeHandle,
    addr: SocketAddr,
    root: SessionId,
    bot: SessionId,
    team: SessionId,
}

impl Harness {
    fn start() -> Harness {
        let dir = tempfile::tempdir().unwrap();
        let guard = isolated_home(dir.path());
        let tokio = tokio::runtime::Builder::new_multi_thread()
            .enable_all()
            .build()
            .unwrap();
        let runtime = tokio
            .block_on(Runtime::start(config_for(dir.path())))
            .unwrap();
        let handle = runtime.handle();

        let root = handle.snapshot().sessions[0].id.clone();
        let bot_id = handle
            .bot_create("alpha", "assistant", "the first bot")
            .unwrap();
        let bot = handle.bot_chat(&bot_id).unwrap();
        let hive = handle
            .hive_create(UserId::new("operator"), None, "alpha", "ship it", "open")
            .unwrap();
        let team = handle.create_session(Some("team alpha".into())).unwrap();
        handle.hive_set_session(&hive, Some(&team)).unwrap();

        Harness {
            _dir: dir,
            _guard: guard,
            tokio,
            runtime,
            handle,
            addr: SocketAddr::from(([127, 0, 0, 1], 0)),
            root,
            bot,
            team,
        }
    }

    /// Mint tokens before the server binds: it loads the store once, at bind time.
    fn mint(&self, name: &str, tier: &str, lifetime: Lifetime) -> String {
        let mut store = TokenStore::at_home().unwrap();
        store
            .add(
                AddRequest {
                    name: name.into(),
                    tier: tier.into(),
                    lifetime,
                    owner: false,
                },
                vikram_link::time::now_ms(),
                vikram_link::token::DEFAULT_MAX_TEMP_MS,
            )
            .unwrap()
            .secret
    }

    fn serve(mut self) -> Harness {
        let server = Server::bind(ServerConfig {
            bind: "127.0.0.1:0".into(),
            tls_cert: None,
            tls_key: None,
            env_token: None,
        })
        .unwrap();
        self.addr = server.local_addr();
        let handle = self.handle.clone();
        let tokio = self.tokio.handle().clone();
        std::thread::spawn(move || {
            let _ = server.serve(handle, tokio);
        });
        self
    }
}

fn request(
    addr: SocketAddr,
    method: &str,
    path: &str,
    token: Option<&str>,
    body: Option<&str>,
) -> (u16, Value) {
    let mut stream = TcpStream::connect(addr).expect("connect");
    stream
        .set_read_timeout(Some(Duration::from_secs(10)))
        .unwrap();
    let mut head = format!("{method} {path} HTTP/1.1\r\nHost: link\r\nConnection: close\r\n");
    if let Some(token) = token {
        head.push_str(&format!("Authorization: Bearer {token}\r\n"));
    }
    if let Some(body) = body {
        head.push_str(&format!(
            "Content-Type: application/json\r\nContent-Length: {}\r\n",
            body.len()
        ));
    }
    head.push_str("\r\n");
    stream.write_all(head.as_bytes()).unwrap();
    if let Some(body) = body {
        stream.write_all(body.as_bytes()).unwrap();
    }
    let mut raw = Vec::new();
    stream.read_to_end(&mut raw).expect("read the answer");
    parse(&raw)
}

fn parse(raw: &[u8]) -> (u16, Value) {
    let text = String::from_utf8_lossy(raw);
    let (head, body) = text.split_once("\r\n\r\n").unwrap_or((&text, ""));
    let status: u16 = head
        .split_whitespace()
        .nth(1)
        .and_then(|code| code.parse().ok())
        .unwrap_or(0);
    (status, serde_json::from_str(body).unwrap_or(Value::Null))
}

fn submit(addr: SocketAddr, session: &SessionId, token: &str) -> (u16, Value) {
    request(
        addr,
        "POST",
        &format!("/sessions/{}/submit", session.as_str()),
        Some(token),
        Some(r#"{"text":"hello"}"#),
    )
}

#[test]
fn every_tier_has_its_allow_deny_matrix() {
    let h = Harness::start();
    let tokens = [
        ("t-admin", "admin"),
        ("t-main", "main"),
        ("t-bots", "bots"),
        ("t-bot", "bot:alpha"),
        ("t-team", "team"),
        ("t-team-one", "team:alpha"),
        ("t-read", "read"),
    ];
    let minted: Vec<(&str, &str, String)> = tokens
        .iter()
        .map(|(name, tier)| (*name, *tier, h.mint(name, tier, Lifetime::Permanent)))
        .collect();
    let h = h.serve();

    // (tier, may submit to the root session, may submit to the bot's session)
    let expected = [
        ("admin", true, true),
        ("main", true, true),
        ("bots", false, true),
        ("bot:alpha", false, true),
        ("team", false, false),
        ("team:alpha", false, false),
        ("read", false, false),
    ];
    for ((name, tier, secret), (_, root_ok, bot_ok)) in minted.iter().zip(expected) {
        assert_eq!(tier, &expected.iter().find(|row| row.0 == *tier).unwrap().0);
        let (root_status, root_body) = submit(h.addr, &h.root, secret);
        let (bot_status, bot_body) = submit(h.addr, &h.bot, secret);
        assert_eq!(
            root_status == 202,
            root_ok,
            "`{tier}` submitting to the root session answered {root_status} {root_body} ({name})"
        );
        assert_eq!(
            bot_status == 202,
            bot_ok,
            "`{tier}` submitting to a bot session answered {bot_status} {bot_body} ({name})"
        );
        if !root_ok && *tier != "read" {
            assert_eq!(root_status, 403, "`{tier}` must be refused, not ignored");
            assert_eq!(root_body["tier"], *tier, "the refusal names the tier");
        }
    }

    // Reading is wider than writing: a bot token sees the bot session and not the root one, and a
    // read token sees everything.
    let (_, bot_view) = request(h.addr, "GET", "/sessions", Some(&minted[2].2), None);
    let ids: Vec<&str> = bot_view["sessions"]
        .as_array()
        .unwrap()
        .iter()
        .map(|row| row["id"].as_str().unwrap())
        .collect();
    assert!(ids.contains(&h.bot.as_str()), "bots sees its own session");
    assert!(
        !ids.contains(&h.root.as_str()),
        "bots never sees the root session"
    );

    let (_, read_view) = request(h.addr, "GET", "/sessions", Some(&minted[6].2), None);
    let ids: Vec<&str> = read_view["sessions"]
        .as_array()
        .unwrap()
        .iter()
        .map(|row| row["id"].as_str().unwrap())
        .collect();
    assert!(ids.contains(&h.root.as_str()), "read mirrors every session");
    assert!(ids.contains(&h.team.as_str()));

    // Administration is the admin tier's alone.
    for (name, tier, secret) in &minted {
        let (status, body) = request(h.addr, "GET", "/link/tokens", Some(secret), None);
        if *tier == "admin" {
            assert_eq!(status, 200, "admin lists tokens");
            assert!(body["tokens"].as_array().is_some());
        } else {
            assert_eq!(status, 403, "`{tier}` must not list tokens ({name})");
            assert_eq!(body["tier"], *tier);
        }
        let (audit_status, _) = request(h.addr, "GET", "/link/audit", Some(secret), None);
        assert_eq!(audit_status == 200, *tier == "admin", "audit is admin-only");
    }
}

#[test]
fn a_read_token_cannot_submit_and_a_bot_token_cannot_reach_a_session() {
    let h = Harness::start();
    let read = h.mint("phone", "read", Lifetime::Permanent);
    let bot = h.mint("buildbot", "bot:alpha", Lifetime::Permanent);
    let h = h.serve();

    let (status, body) = submit(h.addr, &h.root, &read);
    assert_eq!(status, 403, "a read token submits nothing");
    assert_eq!(body["tier"], "read");

    let (status, body) = submit(h.addr, &h.root, &bot);
    assert_eq!(status, 403, "bot:alpha must not reach the root session");
    assert_eq!(body["tier"], "bot:alpha");
    assert!(body["error"].as_str().unwrap().contains("root"));

    let (status, _) = submit(h.addr, &h.bot, &bot);
    assert_eq!(status, 202, "bot:alpha reaches its own conversation");
}

#[test]
fn a_revoked_token_is_refused_with_401() {
    let h = Harness::start();
    let secret = h.mint("laptop", "admin", Lifetime::Permanent);
    let h = h.serve();

    let (status, _) = request(h.addr, "GET", "/sessions", Some(&secret), None);
    assert_eq!(status, 200, "the token works before it is revoked");

    let (status, _) = request(h.addr, "DELETE", "/link/tokens/laptop", Some(&secret), None);
    assert_eq!(status, 200);

    let (status, body) = request(h.addr, "GET", "/sessions", Some(&secret), None);
    assert_eq!(status, 401, "a revoked token is unauthorized: {body}");
    assert!(body["error"].as_str().unwrap().contains("revoked"));
}

#[test]
fn an_expired_token_is_refused_with_401_naming_the_expiry() {
    let h = Harness::start();
    let secret = h.mint("temporary", "admin", Lifetime::For("1s".into()));
    let h = h.serve();

    let (status, _) = request(h.addr, "GET", "/sessions", Some(&secret), None);
    assert_eq!(status, 200, "a temporary token works until it expires");

    std::thread::sleep(Duration::from_millis(1_200));
    let (status, body) = request(h.addr, "GET", "/sessions", Some(&secret), None);
    assert_eq!(status, 401);
    let error = body["error"].as_str().unwrap();
    assert!(error.contains("expired at"), "{error}");
    assert!(body["expired_at"].as_str().unwrap().ends_with('Z'));
}

#[test]
fn a_missing_or_unknown_token_is_401() {
    let h = Harness::start();
    let h = h.serve();
    let (status, body) = request(h.addr, "GET", "/sessions", None, None);
    assert_eq!(status, 401);
    assert!(body["error"].as_str().unwrap().contains("missing token"));

    let (status, _) = request(h.addr, "GET", "/sessions", Some("vlink_nope"), None);
    assert_eq!(status, 401);

    let (status, body) = request(h.addr, "GET", "/health", None, None);
    assert_eq!(status, 200, "liveness needs no token");
    assert_eq!(body["ok"], true);
}

#[test]
fn a_non_loopback_bind_needs_a_bootstrap_secret() {
    let err = Server::bind(ServerConfig {
        bind: "0.0.0.0:0".into(),
        tls_cert: None,
        tls_key: None,
        env_token: None,
    })
    .err()
    .expect("a public bind without a secret must be refused");
    assert!(err.contains("VIKRAM_LINK_TOKEN"), "{err}");
    assert!(err.contains("0.0.0.0"), "{err}");

    let server = Server::bind(ServerConfig {
        bind: "0.0.0.0:0".into(),
        tls_cert: None,
        tls_key: None,
        env_token: Some("bootstrap-secret".into()),
    })
    .expect("a public bind with a bootstrap secret is allowed");
    assert!(!server.local_addr().ip().is_loopback());
}

#[test]
fn the_audit_log_records_actions_and_never_the_secret() {
    let h = Harness::start();
    let admin = h.mint("laptop", "admin", Lifetime::Permanent);
    let read = h.mint("phone", "read", Lifetime::Permanent);
    let h = h.serve();

    let _ = request(h.addr, "GET", "/sessions", Some(&read), None);
    let _ = submit(h.addr, &h.root, &read);
    let (status, body) = request(h.addr, "GET", "/link/audit?limit=50", Some(&admin), None);
    assert_eq!(status, 200);
    let records = body["records"].as_array().unwrap();
    assert!(!records.is_empty(), "actions are recorded");
    assert!(records.iter().any(|row| row["token"] == "phone"
        && row["action"] == "sessions"
        && row["result"] == "ok"));
    assert!(
        records.iter().any(|row| row["action"] == "submit"
            && row["result"].as_str().unwrap().starts_with("denied:")),
        "a refusal is recorded too: {body}"
    );
    assert!(records.iter().any(|row| row["tier"] == "read"));

    let file = std::fs::read_to_string(link_dir().join("audit.jsonl")).unwrap();
    assert!(file.contains("phone"));
    assert!(!file.contains(&read), "the secret must never reach the log");
    assert!(!file.contains(&admin), "no secret is ever written");
    assert!(!file.contains("vlink_"));
}

#[test]
fn tokens_can_be_minted_and_listed_over_http_without_ever_showing_the_secret_again() {
    let h = Harness::start();
    let admin = h.mint("laptop", "admin", Lifetime::Permanent);
    let h = h.serve();

    let (status, created) = request(
        h.addr,
        "POST",
        "/link/tokens",
        Some(&admin),
        Some(r#"{"name":"tablet","tier":"bot:alpha","for":"30m"}"#),
    );
    assert_eq!(status, 201, "{created}");
    let secret = created["secret"].as_str().unwrap().to_owned();
    assert!(secret.starts_with("vlink_"));
    assert_eq!(created["tier"], "bot:alpha");
    assert!(created["expires"].as_str().unwrap().ends_with('Z'));

    let (status, listed) = request(h.addr, "GET", "/link/tokens", Some(&admin), None);
    assert_eq!(status, 200);
    let text = listed.to_string();
    assert!(!text.contains(&secret), "the list never carries a secret");
    let tablet = listed["tokens"]
        .as_array()
        .unwrap()
        .iter()
        .find(|row| row["name"] == "tablet")
        .unwrap();
    assert_eq!(tablet["tier"], "bot:alpha");
    assert_eq!(tablet["permanent"], false);
    assert_eq!(tablet["state"], "active");
    assert!(tablet["expires_in"].as_str().unwrap().starts_with("in "));

    let (status, body) = request(
        h.addr,
        "POST",
        "/link/tokens",
        Some(&admin),
        Some(r#"{"name":"forever","tier":"read"}"#),
    );
    assert_eq!(status, 400, "neither flag is refused: {body}");
    assert!(body["error"].as_str().unwrap().contains("permanent"));

    let (status, body) = request(
        h.addr,
        "POST",
        "/link/tokens",
        Some(&admin),
        Some(r#"{"name":"forever","tier":"read","for":"365d"}"#),
    );
    assert_eq!(status, 400);
    assert!(body["error"].as_str().unwrap().contains("link.max_temp"));

    let file = std::fs::read_to_string(link_dir().join("tokens.json")).unwrap();
    assert!(!file.contains(&secret));
    assert!(file.contains("\"tier\": \"bot:alpha\""));
}

#[test]
fn the_event_stream_delivers_one_runtime_event() {
    let h = Harness::start();
    let admin = h.mint("laptop", "admin", Lifetime::Permanent);
    let h = h.serve();

    let stream = TcpStream::connect(h.addr).unwrap();
    stream
        .set_read_timeout(Some(Duration::from_secs(10)))
        .unwrap();
    let request = tungstenite::http::Request::builder()
        .uri(format!("ws://{}/events?token={}", h.addr, admin))
        .header("Host", h.addr.to_string())
        .header("Connection", "Upgrade")
        .header("Upgrade", "websocket")
        .header("Sec-WebSocket-Version", "13")
        .header("Sec-WebSocket-Key", "dGhlIHNhbXBsZSBub25jZQ==")
        .body(())
        .unwrap();
    let (mut socket, response) = tungstenite::client::client(request, stream).expect("upgrade");
    assert_eq!(response.status(), 101);
    socket
        .get_mut()
        .set_read_timeout(Some(Duration::from_secs(5)))
        .unwrap();

    let created = {
        let _entered = h.tokio.enter();
        h.handle
            .create_session(Some("from a linked device".into()))
            .unwrap()
    };

    let mut seen: Option<Value> = None;
    for _ in 0..50 {
        match socket.read() {
            Ok(message) => {
                let text = message.into_text().unwrap().to_string();
                let row: Value = serde_json::from_str(&text).unwrap();
                if row["type"] == "session_created" && row["session"] == created.as_str() {
                    seen = Some(row);
                    break;
                }
            }
            Err(err) => panic!("the event stream failed: {err}"),
        }
    }
    let row = seen.expect("one SessionCreated must arrive on the stream");
    assert_eq!(row["title"], "from a linked device");
    assert!(row["ts_ms"].as_u64().unwrap() > 0);
}

#[test]
fn a_read_token_may_watch_the_stream_and_a_bot_token_sees_only_its_own_session() {
    let h = Harness::start();
    let bot = h.mint("buildbot", "bot:alpha", Lifetime::Permanent);
    let h = h.serve();

    let stream = TcpStream::connect(h.addr).unwrap();
    stream
        .set_read_timeout(Some(Duration::from_secs(10)))
        .unwrap();
    let request = tungstenite::http::Request::builder()
        .uri(format!("ws://{}/events?token={}", h.addr, bot))
        .header("Host", h.addr.to_string())
        .header("Connection", "Upgrade")
        .header("Upgrade", "websocket")
        .header("Sec-WebSocket-Version", "13")
        .header("Sec-WebSocket-Key", "dGhlIHNhbXBsZSBub25jZQ==")
        .body(())
        .unwrap();
    let (mut socket, _) = tungstenite::client::client(request, stream).expect("upgrade");
    socket
        .get_mut()
        .set_read_timeout(Some(Duration::from_secs(2)))
        .unwrap();

    // The root session moves; the bot token must not hear it.
    {
        let _entered = h.tokio.enter();
        let _ = h.handle.submit(&h.root, "root only".into());
    }
    let deadline = std::time::Instant::now() + Duration::from_millis(600);
    while std::time::Instant::now() < deadline {
        match socket.read() {
            Ok(message) => {
                let row: Value = serde_json::from_str(&message.into_text().unwrap()).unwrap();
                if let Some(session) = row["session"].as_str() {
                    assert_ne!(session, h.root.as_str(), "bot:alpha heard the root session");
                }
            }
            Err(_) => break,
        }
    }
    let _ = h.tokio;
    let _ = h.runtime;
}

#[test]
fn the_old_tier_names_still_reach_the_same_places() {
    let h = Harness::start();
    // `full` is `admin`: the alias is accepted from the flag and from a token file.
    let legacy = h.mint("old-laptop", "full", Lifetime::Permanent);
    let h = h.serve();
    let (status, body) = request(h.addr, "GET", "/link/tokens", Some(&legacy), None);
    assert_eq!(status, 200, "`full` is `admin`: {body}");
    let tiers: Vec<&str> = body["tokens"]
        .as_array()
        .unwrap()
        .iter()
        .map(|row| row["tier"].as_str().unwrap())
        .collect();
    assert!(
        tiers.contains(&"admin"),
        "the store prints the new name: {body}"
    );

    let (status, body) = submit(h.addr, &h.root, &legacy);
    assert_eq!(status, 202, "{body}");
}

#[test]
fn a_json_body_is_required_for_submit() {
    let h = Harness::start();
    let admin = h.mint("laptop", "admin", Lifetime::Permanent);
    let h = h.serve();
    let (status, body) = request(
        h.addr,
        "POST",
        &format!("/sessions/{}/submit", h.root.as_str()),
        Some(&admin),
        Some("not json"),
    );
    assert_eq!(status, 400);
    assert!(body["error"].as_str().unwrap().contains("invalid JSON"));
    let _ = json!({});
}
