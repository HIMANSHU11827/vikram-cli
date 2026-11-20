//! The completion request the turn loop puts on the wire carries the model's declared output
//! cap. These tests boot the real runtime against a loopback endpoint and read the JSON body
//! the provider actually sent — the only place the hook/config/registry fallback order is
//! observable end to end.

#![allow(clippy::await_holding_lock)]

use std::time::Duration;

use vikram_runtime::types::RuntimeEvent;
use vikram_runtime::{Config, Runtime};

fn isolated_home(dir: &std::path::Path, label: &str) -> std::sync::MutexGuard<'static, ()> {
    let guard = vikram_runtime::config::lock_security_env();
    unsafe { std::env::set_var("VIKRAM_HOME", dir.join(format!("home-{label}"))) };
    guard
}

fn key_env(label: &str) -> String {
    let name = format!("VIKRAM_REQCAP_{}_{}", std::process::id(), label).to_uppercase();
    unsafe { std::env::set_var(&name, "sk-test") };
    name
}

/// The byte index just past the blank line that ends an HTTP request head.
fn head_end(buffer: &[u8]) -> Option<usize> {
    buffer
        .windows(4)
        .position(|window| window == b"\r\n\r\n")
        .map(|index| index + 4)
}

fn content_length(head: &[u8]) -> usize {
    String::from_utf8_lossy(head)
        .lines()
        .find_map(|line| {
            let (name, value) = line.split_once(':')?;
            name.trim()
                .eq_ignore_ascii_case("content-length")
                .then(|| value.trim().parse::<usize>().ok())?
        })
        .unwrap_or(0)
}

/// A loopback endpoint that records every request body and answers each with a one-word stream.
/// Returns the port and a channel of the JSON bodies, in arrival order.
async fn canned_endpoint() -> (u16, tokio::sync::mpsc::UnboundedReceiver<String>) {
    use tokio::io::{AsyncReadExt, AsyncWriteExt};

    let listener = tokio::net::TcpListener::bind("127.0.0.1:0")
        .await
        .expect("bind a loopback port");
    let port = listener.local_addr().expect("loopback address").port();
    let (tx, rx) = tokio::sync::mpsc::unbounded_channel();

    tokio::spawn(async move {
        loop {
            let (mut socket, _) = match listener.accept().await {
                Ok(pair) => pair,
                Err(_) => return,
            };
            let recorded = tx.clone();
            tokio::spawn(async move {
                let mut received = Vec::new();
                let mut chunk = [0u8; 4096];
                loop {
                    let read = socket.read(&mut chunk).await.expect("read the request");
                    if read == 0 {
                        break;
                    }
                    received.extend_from_slice(&chunk[..read]);
                    if let Some(end) = head_end(&received)
                        && received.len() >= end + content_length(&received[..end])
                    {
                        break;
                    }
                }
                let raw = String::from_utf8_lossy(&received).into_owned();
                let body = raw.split_once("\r\n\r\n").map(|(_, b)| b).unwrap_or("");
                let _ = recorded.send(body.to_owned());

                let sse = concat!(
                    "data: {\"choices\":[{\"delta\":{\"content\":\"ok\"}}]}\n\n",
                    "data: {\"choices\":[],\"usage\":{\"prompt_tokens\":1,\"completion_tokens\":1}}\n\n",
                    "data: [DONE]\n\n",
                );
                let response = format!(
                    "HTTP/1.1 200 OK\r\nContent-Type: text/event-stream\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{}",
                    sse.len(),
                    sse
                );
                let _ = socket.write_all(response.as_bytes()).await;
                let _ = socket.shutdown().await;
            });
        }
    });

    (port, rx)
}

/// Run one turn and return every request body the endpoint saw, in order.
async fn turn_request_bodies(
    model: &str,
    dir: &std::path::Path,
    label: &str,
) -> Vec<serde_json::Value> {
    let (port, mut bodies) = canned_endpoint().await;
    let env = key_env(label);
    let config = Config {
        base_url: format!("http://127.0.0.1:{port}/v1"),
        model: model.into(),
        api_key_env: env,
        workspace_root: Some(dir.to_path_buf()),
        ..Config::default()
    };
    let runtime = Runtime::start(config).await.expect("runtime starts");
    let handle = runtime.handle();
    let session = handle
        .snapshot()
        .sessions
        .first()
        .expect("a session exists")
        .id
        .clone();
    let mut events = handle.events();
    handle
        .submit(&session, "say the word ok".into())
        .expect("the turn is accepted");

    // The turn's own request is the one carrying the submitted text; a session-naming call may
    // arrive too, so collect until the turn finishes rather than counting connections.
    let deadline = tokio::time::Instant::now() + Duration::from_secs(20);
    loop {
        let Ok(Ok(event)) = tokio::time::timeout(Duration::from_millis(200), events.recv()).await
        else {
            assert!(
                tokio::time::Instant::now() < deadline,
                "the turn never finished"
            );
            continue;
        };
        if matches!(event, RuntimeEvent::TurnFinished { .. }) {
            break;
        }
    }
    // Let the recording task flush the last body before reading the channel dry.
    tokio::time::sleep(Duration::from_millis(100)).await;
    let mut out = Vec::new();
    while let Ok(body) = bodies.try_recv() {
        if let Ok(value) = serde_json::from_str(&body) {
            out.push(value);
        }
    }
    out
}

#[tokio::test]
async fn a_known_models_registry_output_cap_reaches_the_request() {
    let dir = tempfile::tempdir().expect("temp dir");
    let _home = isolated_home(dir.path(), "gpt4o");

    let bodies = turn_request_bodies("gpt-4o", dir.path(), "gpt4o").await;
    let request = bodies
        .iter()
        .find(|body| {
            body["messages"].as_array().is_some_and(|messages| {
                messages.iter().any(|message| {
                    message["role"] == "user" && message["content"] == "say the word ok"
                })
            })
        })
        .expect("the turn's request was sent");

    assert_eq!(
        request["max_tokens"],
        serde_json::json!(16_384),
        "the registry figure for gpt-4o must reach the wire: {request}"
    );
    assert!(
        request.get("max_completion_tokens").is_none(),
        "gpt-4o still takes max_tokens: {request}"
    );
}

#[tokio::test]
async fn a_reasoning_model_gets_max_completion_tokens_with_the_registry_cap() {
    let dir = tempfile::tempdir().expect("temp dir");
    let _home = isolated_home(dir.path(), "o3mini");

    let bodies = turn_request_bodies("o3-mini", dir.path(), "o3mini").await;
    let request = bodies
        .iter()
        .find(|body| {
            body["messages"].as_array().is_some_and(|messages| {
                messages.iter().any(|message| {
                    message["role"] == "user" && message["content"] == "say the word ok"
                })
            })
        })
        .expect("the turn's request was sent");

    assert_eq!(
        request["max_completion_tokens"],
        serde_json::json!(100_000),
        "o3-mini's registry cap must reach the wire under the field it accepts: {request}"
    );
    assert!(
        request.get("max_tokens").is_none(),
        "o3-mini rejects max_tokens: {request}"
    );
}
