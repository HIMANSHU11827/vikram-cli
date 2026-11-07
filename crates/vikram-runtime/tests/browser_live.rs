//! Live browser tests: these spawn the installed Chromium and drive a real page over CDP.
//!
//! They skip (return early) when no browser is installed, so a machine without one stays green;
//! on a machine with one they must actually exercise it — there is no `#[ignore]` here.

use std::io::{Read, Write};
use std::net::TcpListener;
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::thread;
use std::time::Duration;

use vikram_runtime::browser::{cdp, screenshot};

const PAGE: &str = r#"<!doctype html>
<html><head><title>Vikram Browser Test</title></head>
<body>
  <h1 id="heading">hello from a real browser</h1>
  <button id="button" onclick="document.getElementById('result').textContent='clicked'; document.body.dataset.clicked='yes'">press</button>
  <input id="field" />
  <div id="result">not clicked</div>
  <div id="typed"></div>
  <script>
    document.getElementById('field').addEventListener('input', (event) => {
      document.getElementById('typed').textContent = event.target.value;
    });
  </script>
</body></html>"#;

struct Server {
    port: u16,
    stop: Arc<AtomicBool>,
    handle: Option<thread::JoinHandle<()>>,
}

impl Server {
    fn start() -> Self {
        let listener = TcpListener::bind("127.0.0.1:0").expect("bind local test server");
        let port = listener.local_addr().expect("local addr").port();
        listener
            .set_nonblocking(true)
            .expect("nonblocking listener");
        let stop = Arc::new(AtomicBool::new(false));
        let stop_thread = Arc::clone(&stop);
        let handle = thread::spawn(move || {
            while !stop_thread.load(Ordering::Relaxed) {
                match listener.accept() {
                    Ok((mut stream, _)) => {
                        let mut buffer = [0u8; 2048];
                        let _ = stream.read(&mut buffer);
                        let body = PAGE.as_bytes();
                        let response = format!(
                            "HTTP/1.1 200 OK\r\nContent-Type: text/html; charset=utf-8\r\nContent-Length: {}\r\nConnection: close\r\n\r\n",
                            body.len()
                        );
                        let _ = stream.write_all(response.as_bytes());
                        let _ = stream.write_all(body);
                        let _ = stream.flush();
                    }
                    Err(err) if err.kind() == std::io::ErrorKind::WouldBlock => {
                        thread::sleep(Duration::from_millis(5));
                    }
                    Err(_) => break,
                }
            }
        });
        Self {
            port,
            stop,
            handle: Some(handle),
        }
    }

    fn url(&self) -> String {
        format!("http://127.0.0.1:{}/", self.port)
    }
}

impl Drop for Server {
    fn drop(&mut self) {
        self.stop.store(true, Ordering::Relaxed);
        if let Some(handle) = self.handle.take() {
            let _ = handle.join();
        }
    }
}

struct Session(Option<String>);

impl Session {
    fn open(url: &str) -> Result<Self, String> {
        Ok(Self(Some(cdp::open(url)?)))
    }

    fn id(&self) -> &str {
        self.0.as_deref().unwrap_or_default()
    }
}

impl Drop for Session {
    fn drop(&mut self) {
        if let Some(id) = self.0.take() {
            let _ = cdp::close(&id);
        }
    }
}

fn open_or_skip(url: &str) -> Option<Session> {
    match Session::open(url) {
        Ok(session) => Some(session),
        Err(err) if err.contains("no supported browser") || err.contains("VIKRAM_CHROMIUM") => {
            eprintln!("skipping live browser test: {err}");
            None
        }
        Err(err) => panic!("browser failed to open {url}: {err}"),
    }
}

#[test]
fn a_local_page_loads_and_evaluates_in_the_browser() {
    let server = Server::start();
    let Some(session) = open_or_skip(&server.url()) else {
        return;
    };
    let title = cdp::eval(session.id(), "document.title").expect("evaluate document.title");
    assert!(
        title.contains("Vikram Browser Test"),
        "unexpected title: {title}"
    );
    let heading = cdp::eval(
        session.id(),
        "document.getElementById('heading').textContent",
    )
    .expect("read heading");
    assert!(heading.contains("hello from a real browser"), "{heading}");
    let body = cdp::text(session.id()).expect("read page text");
    assert!(body.contains("hello from a real browser"), "{body}");
}

#[test]
fn a_real_click_updates_the_dom() {
    let server = Server::start();
    let Some(session) = open_or_skip(&server.url()) else {
        return;
    };
    let before = cdp::eval(
        session.id(),
        "document.getElementById('result').textContent",
    )
    .expect("read result before click");
    assert_eq!(before, "not clicked");
    cdp::click(session.id(), "#button").expect("trusted click");
    let after = cdp::eval(
        session.id(),
        "document.getElementById('result').textContent",
    )
    .expect("read result after click");
    assert_eq!(after, "clicked");
    let trusted =
        cdp::eval(session.id(), "document.body.dataset.clicked").expect("read click marker");
    assert_eq!(trusted, "yes");
}

#[test]
fn typing_reaches_the_page_as_input_events() {
    let server = Server::start();
    let Some(session) = open_or_skip(&server.url()) else {
        return;
    };
    cdp::type_text(session.id(), "#field", "vikram").expect("type into the field");
    let value = cdp::eval(session.id(), "document.getElementById('field').value")
        .expect("read input value");
    assert_eq!(value, "vikram");
    let echoed = cdp::eval(session.id(), "document.getElementById('typed').textContent")
        .expect("read input listener output");
    assert_eq!(echoed, "vikram");
}

#[test]
fn a_screenshot_is_a_real_png_with_the_requested_size() {
    let server = Server::start();
    let dir = std::env::temp_dir().join(format!(
        "vikram-browser-shot-{}-{}",
        std::process::id(),
        std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map(|d| d.as_nanos())
            .unwrap_or(0)
    ));
    std::fs::create_dir_all(&dir).expect("temp dir");
    let path = dir.join("page.png");
    let result = screenshot(&server.url(), &path, 800, 600);
    match result {
        Ok(path) => {
            let bytes = std::fs::read(&path).expect("read screenshot");
            assert!(
                bytes.len() > 8 && bytes[..8] == [0x89, b'P', b'N', b'G', 0x0d, 0x0a, 0x1a, 0x0a],
                "not a PNG: {} bytes",
                bytes.len()
            );
            // IHDR width/height live at bytes 16..24, big-endian.
            let width = u32::from_be_bytes([bytes[16], bytes[17], bytes[18], bytes[19]]);
            let height = u32::from_be_bytes([bytes[20], bytes[21], bytes[22], bytes[23]]);
            assert_eq!((width, height), (800, 600), "unexpected PNG size");
        }
        Err(err) if err.contains("no supported browser") => {
            eprintln!("skipping live screenshot test: {err}");
        }
        Err(err) => panic!("screenshot failed: {err}"),
    }
    let _ = std::fs::remove_dir_all(&dir);
}

#[test]
fn an_unreachable_page_fails_instead_of_hanging() {
    // Port 1 on loopback is closed; `open` must report an error, not spin forever.
    let started = std::time::Instant::now();
    match cdp::open("http://127.0.0.1:1/") {
        Ok(session) => {
            let _ = cdp::close(&session);
            panic!("opening a closed port unexpectedly succeeded");
        }
        Err(err) => {
            assert!(
                started.elapsed() < Duration::from_secs(60),
                "failure took {:?}",
                started.elapsed()
            );
            assert!(!err.is_empty(), "empty error");
        }
    }
}
