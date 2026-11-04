use super::*;

/// How long one CDP round trip may wait for its reply before the call is abandoned.
pub const CALL_TIMEOUT_MS: u64 = 20_000;

/// How long [`open`] waits for the browser to publish its debugger port.
const START_TIMEOUT: Duration = Duration::from_secs(20);

/// How long [`open`] waits for the first page to finish loading.
const LOAD_TIMEOUT_MS: u64 = 15_000;

/// Poll interval while waiting for the port file or for `document.readyState`.
const POLL_INTERVAL: Duration = Duration::from_millis(25);

/// The file Chromium writes its chosen port and browser endpoint into, inside the profile dir.
const DEVTOOLS_FILE: &str = "DevToolsActivePort";

/// Where the browser's noise is captured, so a failure can quote it.
const STDERR_FILE: &str = "chromium-stderr.log";

/// How many bytes of that log are read back when something goes wrong.
const STDERR_BYTES: u64 = 8 * 1024;

/// Longest a single reply may be before it is treated as not-JSON. A `document.body.innerText`
/// dump is the only thing that gets near it.
const MAX_REPLY_BYTES: usize = 8 * 1024 * 1024;

/// Longest the debugger's HTTP reply may be. `/json/list` for a browser with many tabs is a few
/// kilobytes; this only stops a wedged endpoint from filling memory.
const MAX_HTTP_BYTES: usize = 4 * 1024 * 1024;

/// One live page, kept in the process-global registry.
struct Session {
    id: String,
    /// The last URL this session navigated to.
    url: String,
    /// The DevTools WebSocket endpoint, for diagnostics.
    endpoint: String,
    child: Child,
    socket: WebSocket<TcpStream>,
    /// Monotonic CDP request id; every reply is matched against it.
    next_id: u64,
    /// The throwaway profile Chromium is running with, removed when the session ends.
    profile: PathBuf,
}

impl Session {
    /// Send one CDP request and wait for the reply that carries its id.
    ///
    /// Events and replies to other ids are discarded rather than buffered: nothing here
    /// subscribes to events, and a buffer would only grow.
    fn call(&mut self, method: &str, params: Value, deadline: Instant) -> Result<Value, Failure> {
        self.next_id += 1;
        let id = self.next_id;
        let request = json!({"id": id, "method": method, "params": params});
        let text = serde_json::to_string(&request)
            .map_err(|e| Failure::Transport(format!("cannot encode `{method}`: {e}")))?;
        self.socket
            .send(tungstenite::Message::text(text))
            .map_err(|e| Failure::Transport(format!("cannot send `{method}`: {e}")))?;
        loop {
            if Instant::now() >= deadline {
                return Err(Failure::Transport(format!(
                    "`{method}` got no reply within {} ms",
                    CALL_TIMEOUT_MS
                )));
            }
            let message = self.socket.read().map_err(|e| {
                Failure::Transport(format!("the connection failed during `{method}`: {e}"))
            })?;
            let text = match message {
                tungstenite::Message::Text(text) => text.to_string(),
                tungstenite::Message::Binary(bytes) => String::from_utf8_lossy(&bytes).into_owned(),
                tungstenite::Message::Close(_) => {
                    return Err(Failure::Transport(format!(
                        "the browser closed the connection during `{method}`"
                    )));
                }
                _ => continue,
            };
            if text.len() > MAX_REPLY_BYTES {
                continue;
            }
            let Ok(value) = serde_json::from_str::<Value>(&text) else {
                continue;
            };
            if value.get("id").and_then(Value::as_u64) != Some(id) {
                continue;
            }
            if let Some(error) = value.get("error") {
                return Err(Failure::Protocol(format!(
                    "{method} was rejected: {}",
                    describe_cdp_error(error)
                )));
            }
            return Ok(value.get("result").cloned().unwrap_or(Value::Null));
        }
    }

    /// `Runtime.evaluate`, returning the reply's `result` object.
    fn evaluate(&mut self, js: &str, deadline: Instant) -> Result<Value, Failure> {
        let result = self.call(
            "Runtime.evaluate",
            json!({
                "expression": js,
                "returnByValue": true,
                "awaitPromise": true,
                "userGesture": true
            }),
            deadline,
        )?;
        if result.get("exceptionDetails").is_some() {
            return Err(Failure::Protocol(format!(
                "the page threw: {}",
                describe_exception(&result)
            )));
        }
        Ok(result)
    }

    /// Say goodbye on the wire, then let [`Drop`] stop the process.
    fn shutdown(mut self) {
        let _ = self.socket.close(None);
        let _ = self.socket.flush();
    }
}

/// A session that goes out of scope — closed, failed, or discarded by a poisoned registry —
/// must not leave a browser process or a profile directory behind.
impl Drop for Session {
    fn drop(&mut self) {
        let _ = self.child.kill();
        let _ = self.child.wait();
        remove_profile(&self.profile);
    }
}

/// Why a CDP call did not produce a value.
///
/// The distinction matters: a transport failure means the socket is unusable and the session
/// must die, while a protocol failure means the browser answered and simply refused the request
/// (a bad selector, a throwing script), so the session is still perfectly good.
enum Failure {
    Transport(String),
    Protocol(String),
}

impl Failure {
    fn into_message(self) -> String {
        match self {
            Failure::Transport(message) | Failure::Protocol(message) => message,
        }
    }
}

fn registry() -> &'static Mutex<HashMap<String, Session>> {
    static REGISTRY: OnceLock<Mutex<HashMap<String, Session>>> = OnceLock::new();
    REGISTRY.get_or_init(|| Mutex::new(HashMap::new()))
}

fn counter() -> &'static AtomicU64 {
    static COUNTER: AtomicU64 = AtomicU64::new(0);
    &COUNTER
}

/// The id handed out for the `n`-th session of this process.
fn session_id(n: u64) -> String {
    format!("browser-{n}")
}

// --- public surface ------------------------------------------------------------------------

/// Open `url` in a fresh headless browser and return the session id.
///
/// The session stays alive until [`close`] or until the process ends, so the caller can click,
/// type and read across several tool calls. Chromium is spawned with `--remote-debugging-port=0`
/// and a throwaway profile, so it cannot collide with — or write to — a browser the user is
/// already running.
pub fn open(url: &str) -> Result<String, String> {
    let bin = available().ok_or_else(|| MISSING_BROWSER.to_string())?;
    open_with(&bin, url)
}

/// [`open`] against a named binary, so the failure paths can be exercised without a browser.
pub fn open_with(bin: &Path, url: &str) -> Result<String, String> {
    let profile = profile_dir()?;
    let mut child = match spawn(bin, &profile) {
        Ok(child) => child,
        Err(e) => {
            let _ = std::fs::remove_dir_all(&profile);
            return Err(e);
        }
    };
    // Every step below owns the child: a failure midway leaves nothing running.
    let port = match wait_for_port(&profile, &mut child, START_TIMEOUT) {
        Ok(found) => found,
        Err(e) => return Err(abandon(child, &profile, e)),
    };
    let endpoint = match page_endpoint(port) {
        Ok(endpoint) => endpoint,
        Err(e) => return Err(abandon(child, &profile, e)),
    };
    let timeout = Duration::from_millis(CALL_TIMEOUT_MS);
    let socket = match connect(&endpoint, port, timeout) {
        Ok(socket) => socket,
        Err(e) => return Err(abandon(child, &profile, e)),
    };

    let mut session = Session {
        id: String::new(),
        url: url.to_string(),
        endpoint,
        child,
        socket,
        next_id: 0,
        profile,
    };
    let deadline = Instant::now() + timeout;
    // `Page.enable` is optional for navigating, so a browser that refuses it is not fatal.
    let _ = session.call("Page.enable", json!({}), deadline);
    if let Err(e) = navigate_to(&mut session, url, deadline) {
        session.shutdown();
        return Err(e);
    }

    let id = session_id(counter().fetch_add(1, Ordering::Relaxed) + 1);
    session.id = id.clone();
    let mut reg = registry()
        .lock()
        .map_err(|_| "browser session registry poisoned".to_string())?;
    reg.insert(id.clone(), session);
    Ok(id)
}

/// Evaluate `js` in the session's page and return the result as a string.
///
/// A string result comes back verbatim; anything else is serialized as JSON, so a script that
/// returns an object or an array arrives as text the model can read.
pub fn eval(session: &str, js: &str) -> Result<String, String> {
    if js.trim().is_empty() {
        return Err("nothing to evaluate: `js` is empty".to_string());
    }
    with_session(session, |s| {
        let result = s.evaluate(js, call_deadline())?;
        Ok(describe_value(result.get("result")))
    })
}

/// The centre of the first element matching `selector`, in CSS pixels from the viewport's
/// top-left corner.
///
/// Script only *measures* here: no event is dispatched, so nothing observable happens to the
/// page. The measurement is the input to a trusted mouse event, which is why an element with no
/// area is an error rather than a click at a guess.
pub fn element_center(session: &str, selector: &str) -> Result<(f64, f64), String> {
    require_non_empty(selector, "selector")?;
    with_session(session, |s| {
        let rect = measure(s, selector)?;
        Ok((rect.x, rect.y))
    })
}

/// Click the first element matching `selector` with a real mouse press and release.
///
/// `HTMLElement.click()` synthesises an untrusted event: `event.isTrusted` is false, so a
/// handler that checks it never runs, a `<canvas>` sees nothing, and the browser's own handling
/// of the control (a `<select>` opening, a checkbox toggling) is bypassed entirely. This
/// dispatches `mousePressed` and `mouseReleased` at the element's own centre instead, which is
/// the pair the browser turns into a click.
pub fn click(session: &str, selector: &str) -> Result<String, String> {
    require_non_empty(selector, "selector")?;
    with_session(session, |s| {
        // The coordinates are viewport-relative, so the element has to be on screen before it
        // can have any: scroll first, then measure what the scroll produced.
        scroll_into_view(s, selector)?;
        let rect = measure(s, selector)?;
        for kind in ["mousePressed", "mouseReleased"] {
            s.call(
                "Input.dispatchMouseEvent",
                mouse_params(kind, rect.x, rect.y),
                call_deadline(),
            )?;
        }
        Ok(format!("clicked {selector} at {:.0},{:.0}", rect.x, rect.y))
    })
}

/// Focus the first element matching `selector` and type `text` into it as trusted input.
///
/// Focus and the emptying of a previous value are done by script — neither is an event, so
/// neither can be observed or refused — but the characters themselves arrive through
/// `Input.insertText`, which is the path the browser's own input method uses: `beforeinput` and
/// `input` fire, `isTrusted` is true, and a framework tracking its own copy of the value (React,
/// Vue, an input mask) sees every change. A contenteditable is left as it is rather than
/// emptied, because clearing it would also throw away the caret the keystrokes need.
pub fn type_text(session: &str, selector: &str, text: &str) -> Result<String, String> {
    require_non_empty(selector, "selector")?;
    with_session(session, |s| {
        focus(s, selector)?;
        clear_value(s, selector)?;
        insert_text(s, text)?;
        Ok(format!(
            "typed {} characters into {selector}",
            text.chars().count()
        ))
    })
}

/// Scroll the page by `x` pixels right and `y` pixels down, and report the delta applied.
///
/// A wheel event rather than `window.scrollBy`: a page that listens for `wheel` — an infinite
/// scroller, a virtualised list, a map — scrolls itself in response and can clamp, snap or
/// refuse, all of which `scrollBy` flatly overrides.
pub fn scroll(session: &str, x: i64, y: i64) -> Result<String, String> {
    with_session(session, |s| {
        s.call(
            "Input.dispatchMouseEvent",
            wheel_params(x, y),
            call_deadline(),
        )?;
        Ok(format!("scrolled by {x},{y} pixels"))
    })
}

/// Press and release a key combination: `Enter`, `Tab`, `escape`, `ctrl+l`, `shift+tab`.
///
/// Best-effort by design. The parser understands the four modifiers and the named keys listed
/// in `named_key`, and an unrecognised multi-character name is still sent as a key of that name,
/// so a new key in a future browser costs nothing. Whether the *page* reacts is the caller's
/// business: a key nothing listens for is not an error.
pub fn key(session: &str, combo: &str) -> Result<String, String> {
    require_non_empty(combo, "combo")?;
    let stroke =
        parse_key_combo(combo).ok_or_else(|| format!("cannot parse key combo `{combo}`"))?;
    with_session(session, |s| {
        s.call(
            "Input.dispatchKeyEvent",
            stroke.down_params(),
            call_deadline(),
        )?;
        s.call(
            "Input.dispatchKeyEvent",
            stroke.up_params(),
            call_deadline(),
        )?;
        Ok(format!("pressed `{combo}`"))
    })
}

/// The rendered, visible text of the page (`document.body.innerText`).
pub fn text(session: &str) -> Result<String, String> {
    eval(session, &js_text())
}

/// Kill the browser behind `session` and forget it.
pub fn close(session: &str) -> Result<(), String> {
    let dead = {
        let mut reg = registry()
            .lock()
            .map_err(|_| "browser session registry poisoned".to_string())?;
        reg.remove(session)
            .ok_or_else(|| format!("unknown browser session `{session}`"))?
    };
    dead.shutdown();
    Ok(())
}

/// The ids of every live session, sorted.
pub fn list_sessions() -> Vec<String> {
    let reg = match registry().lock() {
        Ok(reg) => reg,
        Err(_) => return Vec::new(),
    };
    let mut ids: Vec<String> = reg.keys().cloned().collect();
    ids.sort();
    ids
}

/// One line per live session: its id, the page it is on, and the debugger endpoint.
pub fn describe_sessions() -> Vec<String> {
    let reg = match registry().lock() {
        Ok(reg) => reg,
        Err(_) => return Vec::new(),
    };
    let mut lines: Vec<String> = reg
        .values()
        .map(|session| format!("{} {} ({})", session.id, session.url, session.endpoint))
        .collect();
    lines.sort();
    lines
}

/// How many sessions are live. Cheap enough to call from a status line.
pub fn count() -> usize {
    registry().lock().map(|reg| reg.len()).unwrap_or(0)
}

// --- session plumbing ----------------------------------------------------------------------

fn call_deadline() -> Instant {
    Instant::now() + Duration::from_millis(CALL_TIMEOUT_MS)
}

fn require_non_empty(value: &str, name: &str) -> Result<(), String> {
    if value.trim().is_empty() {
        return Err(format!("`{name}` must not be empty"));
    }
    Ok(())
}

// --- the steps a trusted interaction is built from -----------------------------------------

/// Scroll the first element matching `selector` to the middle of the viewport.
///
/// Separate from [`measure`] rather than fused with it because the measurement has to be taken
/// *after* the scroll: `getBoundingClientRect` reports where the element is now, and a click
/// dispatched at the old coordinates lands wherever the element used to be.
fn scroll_into_view(session: &mut Session, selector: &str) -> Result<(), Failure> {
    let reply = session.evaluate(&js_scroll_into_view(selector), call_deadline())?;
    match reply_value(&reply) {
        Some(Value::Bool(true)) => Ok(()),
        _ => Err(no_element(selector)),
    }
}

/// Measure the first element matching `selector`, refusing one that occupies no space.
fn measure(session: &mut Session, selector: &str) -> Result<Rect, Failure> {
    let reply = session.evaluate(&js_element_center(selector), call_deadline())?;
    let rect = parse_rect(reply.get("result")).ok_or_else(|| no_element(selector))?;
    if !rect.has_area() {
        return Err(Failure::Protocol(format!(
            "element has no size: {selector}"
        )));
    }
    Ok(rect)
}

/// Focus the first element matching `selector`, or fail when nothing matches.
fn focus(session: &mut Session, selector: &str) -> Result<(), Failure> {
    let reply = session.evaluate(&js_focus(selector), call_deadline())?;
    match reply_value(&reply) {
        Some(Value::Bool(true)) => Ok(()),
        _ => Err(no_element(selector)),
    }
}

/// Empty an `<input>`/`<textarea>` before trusted typing replaces its value.
///
/// A script assignment fires no event, which is harmless here: the characters typed next are
/// trusted input events that carry the *new* value, so whatever tracks the field catches up on
/// the first keystroke. The alternative — selecting all and pressing a key — would leave a
/// selection behind on a page that ignores the key.
fn clear_value(session: &mut Session, selector: &str) -> Result<(), Failure> {
    session.evaluate(&js_clear_value(selector), call_deadline())?;
    Ok(())
}

/// Type `text` as trusted input: one `Input.insertText`, or character events if that is refused.
fn insert_text(session: &mut Session, text: &str) -> Result<(), Failure> {
    if text.is_empty() {
        return Ok(());
    }
    match session.call("Input.insertText", json!({ "text": text }), call_deadline()) {
        Ok(_) => Ok(()),
        // An older or trimmed protocol surface may not implement `insertText`. One `char`
        // event per character produces the same `beforeinput`/`input` pair; it is slower, but
        // it is the same trusted path rather than a fallback to setting `.value`.
        Err(Failure::Protocol(_)) => type_characters(session, text),
        Err(message) => Err(message),
    }
}

/// The last resort for typing: one `char` key event per character.
fn type_characters(session: &mut Session, text: &str) -> Result<(), Failure> {
    for ch in text.chars() {
        let mut buffer = [0u8; 4];
        let params = json!({"type": "char", "text": ch.encode_utf8(&mut buffer)});
        session.call("Input.dispatchKeyEvent", params, call_deadline())?;
    }
    Ok(())
}

/// The error a selector that matches nothing produces, naming the selector.
fn no_element(selector: &str) -> Failure {
    Failure::Protocol(format!("selector matched no element: {selector}"))
}

/// Run `f` against the session's socket without holding the registry lock.
///
/// The session is taken out of the registry for the duration of the call, so a slow page cannot
/// block [`list_sessions`], and put back if the socket survived. A transport failure means the
/// socket is broken: the browser is killed and the session disappears, which the caller sees as
/// a plain error rather than a session that fails forever.
fn with_session<T>(
    id: &str,
    f: impl FnOnce(&mut Session) -> Result<T, Failure>,
) -> Result<T, String> {
    let mut session = {
        let mut reg = registry()
            .lock()
            .map_err(|_| "browser session registry poisoned".to_string())?;
        reg.remove(id)
            .ok_or_else(|| format!("unknown browser session `{id}`"))?
    };
    match f(&mut session) {
        Ok(value) => {
            let id = session.id.clone();
            if let Ok(mut reg) = registry().lock() {
                reg.insert(id, session);
            }
            Ok(value)
        }
        Err(Failure::Protocol(message)) => {
            let id = session.id.clone();
            if let Ok(mut reg) = registry().lock() {
                reg.insert(id, session);
            }
            Err(message)
        }
        Err(Failure::Transport(message)) => {
            session.shutdown();
            Err(message)
        }
    }
}

/// Kill a half-built session and return `message`, so a failed [`open`] leaves no process.
fn abandon(mut child: Child, profile: &Path, message: String) -> String {
    let _ = child.kill();
    let _ = child.wait();
    remove_profile(profile);
    message
}

/// Delete a profile directory, retrying briefly.
///
/// Chromium's helper processes can still be flushing into the profile after the browser
/// process is gone — they re-create `Default/Cache` and friends — so a single `remove_dir_all`
/// is followed by the directory coming back. Nothing depends on it being gone, so a persistent
/// failure is ignored rather than reported; the retries just stop the usual case from leaving
/// litter in the temporary directory.
fn remove_profile(profile: &Path) {
    for attempt in 0..6 {
        std::thread::sleep(Duration::from_millis(if attempt == 0 { 50 } else { 60 }));
        let _ = std::fs::remove_dir_all(profile);
        if !profile.exists() {
            return;
        }
    }
    let _ = std::fs::remove_dir_all(profile);
}

/// A fresh, empty profile directory. Chromium refuses `--remote-debugging-port` with the default
/// one (and would otherwise write to the user's real profile).
fn profile_dir() -> Result<PathBuf, String> {
    let nanos = SystemTime::now()
        .duration_since(SystemTime::UNIX_EPOCH)
        .map(|d| d.as_nanos())
        .unwrap_or(0);
    let profile = std::env::temp_dir().join(format!(
        "vikram-cdp-{}-{nanos}-{}",
        std::process::id(),
        counter().load(Ordering::Relaxed)
    ));
    std::fs::create_dir_all(&profile)
        .map_err(|e| format!("cannot create `{}`: {e}", profile.display()))?;
    Ok(profile)
}

/// Spawn headless Chromium with the debugger on port 0 (the OS picks a free one).
fn spawn(bin: &Path, profile: &Path) -> Result<Child, String> {
    let log = std::fs::File::create(profile.join(STDERR_FILE)).map_err(|e| {
        format!(
            "cannot write `{}`: {e}",
            profile.join(STDERR_FILE).display()
        )
    })?;
    Command::new(bin)
        .arg("--headless=new")
        .arg("--disable-gpu")
        .arg("--no-sandbox")
        .arg("--no-first-run")
        .arg("--no-default-browser-check")
        .arg("--disable-background-networking")
        .arg("--disable-extensions")
        .arg("--disable-dev-shm-usage")
        .arg("--mute-audio")
        .arg("--hide-scrollbars")
        .arg("--remote-debugging-port=0")
        .arg(format!("--user-data-dir={}", profile.display()))
        // A page target has to exist before the debugger can be attached to one; asking for
        // `about:blank` is the cheapest way to guarantee that.
        .arg("about:blank")
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::from(log))
        .spawn()
        .map_err(|e| format!("cannot start `{}`: {e}", bin.display()))
}

/// Wait for `DevToolsActivePort`, quoting the browser's log if it never appears.
fn wait_for_port(profile: &Path, child: &mut Child, timeout: Duration) -> Result<u16, String> {
    let path = profile.join(DEVTOOLS_FILE);
    let deadline = Instant::now() + timeout;
    loop {
        if let Ok(text) = std::fs::read_to_string(&path) {
            if let Some((port, _endpoint)) = parse_devtools_port(&text) {
                return Ok(port);
            }
        }
        if let Ok(Some(status)) = child.try_wait() {
            return Err(format!(
                "the browser exited with {} before it opened a debugger port{}",
                describe_code(status.code()),
                describe_stderr(&read_log(profile))
            ));
        }
        if Instant::now() >= deadline {
            return Err(format!(
                "the browser opened no debugger port within {} ms{}",
                timeout.as_millis(),
                describe_stderr(&read_log(profile))
            ));
        }
        std::thread::sleep(POLL_INTERVAL);
    }
}

/// The browser's last words, so a startup failure says *why*.
fn read_log(profile: &Path) -> String {
    let Ok(mut file) = std::fs::File::open(profile.join(STDERR_FILE)) else {
        return String::new();
    };
    let len = file.metadata().map(|m| m.len()).unwrap_or(0);
    let tail = len.saturating_sub(STDERR_BYTES);
    if tail > 0 && std::io::Seek::seek(&mut file, std::io::SeekFrom::Start(tail)).is_err() {
        return String::new();
    }
    let mut buffer = Vec::new();
    if file.take(STDERR_BYTES).read_to_end(&mut buffer).is_err() {
        return String::new();
    }
    String::from_utf8_lossy(&buffer).into_owned()
}

/// The WebSocket URL of the browser's first page target, from the DevTools HTTP endpoint.
fn page_endpoint(port: u16) -> Result<String, String> {
    let body = http_get(port, "/json/list")?;
    let targets: Value = serde_json::from_str(&body)
        .map_err(|e| format!("the browser's target list was not JSON: {e}"))?;
    let empty = Vec::new();
    let targets = targets.as_array().unwrap_or(&empty);
    for target in targets {
        if target.get("type").and_then(Value::as_str) != Some("page") {
            continue;
        }
        if let Some(url) = target.get("webSocketDebuggerUrl").and_then(Value::as_str) {
            if !url.is_empty() {
                return Ok(url.to_string());
            }
        }
    }
    let kinds: Vec<&str> = targets
        .iter()
        .filter_map(|t| t.get("type").and_then(Value::as_str))
        .collect();
    Err(format!(
        "the browser exposes no page target (it offered: {})",
        if kinds.is_empty() {
            "nothing".to_string()
        } else {
            kinds.join(", ")
        }
    ))
}

/// A minimal HTTP/1.1 GET against the loopback DevTools endpoint.
///
/// Hand-rolled rather than routed through the crate's HTTP client: the endpoint is plain
/// `http://127.0.0.1`, and a blocking client built inside a runtime thread is a panic waiting
/// to happen. This request cannot leave the machine.
fn http_get(port: u16, path: &str) -> Result<String, String> {
    let address = SocketAddr::from(([127, 0, 0, 1], port));
    let mut stream = TcpStream::connect_timeout(&address, Duration::from_secs(5))
        .map_err(|e| format!("cannot reach the browser's debugger on 127.0.0.1:{port}: {e}"))?;
    stream
        .set_read_timeout(Some(Duration::from_secs(10)))
        .map_err(|e| format!("cannot configure the debugger connection: {e}"))?;
    let request = format!(
        "GET {path} HTTP/1.1\r\nHost: 127.0.0.1:{port}\r\nAccept: application/json\r\n\
             Connection: close\r\n\r\n"
    );
    stream
        .write_all(request.as_bytes())
        .map_err(|e| format!("cannot ask the browser for {path}: {e}"))?;
    // Chromium answers with `Content-Length` and then leaves the socket open, so the reply is
    // read until it is *complete* rather than until the peer hangs up.
    let mut raw: Vec<u8> = Vec::new();
    let mut buffer = [0u8; 8192];
    loop {
        match stream.read(&mut buffer) {
            Ok(0) => break,
            Ok(read) => {
                raw.extend_from_slice(&buffer[..read]);
                if raw.len() > MAX_HTTP_BYTES {
                    return Err(format!("the browser's reply to {path} was too large"));
                }
                if response_is_complete(&raw) {
                    break;
                }
            }
            Err(e) => {
                if response_is_complete(&raw) {
                    break;
                }
                return Err(format!("cannot read the browser's reply to {path}: {e}"));
            }
        }
    }
    parse_http_response(&String::from_utf8_lossy(&raw))
}

/// Connect a WebSocket to a `ws://` endpoint, bounding every read and write by `timeout`.
fn connect(endpoint: &str, port: u16, timeout: Duration) -> Result<WebSocket<TcpStream>, String> {
    match parse_ws_url(endpoint) {
        Some((_host, advertised, _path)) if advertised == port => {}
        Some((_host, advertised, _path)) => {
            return Err(format!(
                "the browser advertised `{endpoint}` on port {advertised}, not {port}"
            ));
        }
        None => return Err(format!("`{endpoint}` is not a ws:// URL")),
    }
    let address = SocketAddr::from(([127, 0, 0, 1], port));
    let stream = TcpStream::connect_timeout(&address, timeout)
        .map_err(|e| format!("cannot connect to the debugger socket: {e}"))?;
    stream
        .set_read_timeout(Some(timeout))
        .map_err(|e| format!("cannot configure the debugger socket: {e}"))?;
    stream
        .set_write_timeout(Some(timeout))
        .map_err(|e| format!("cannot configure the debugger socket: {e}"))?;
    tungstenite::client::client(endpoint, stream)
        .map(|(socket, _response)| socket)
        .map_err(|e| format!("the debugger handshake failed: {e}"))
}

/// Navigate an open session and give the page a chance to finish loading.
fn navigate_to(session: &mut Session, url: &str, deadline: Instant) -> Result<(), String> {
    let result = session
        .call("Page.navigate", json!({"url": url}), deadline)
        .map_err(Failure::into_message)?;
    if let Some(problem) = result.get("errorText").and_then(Value::as_str) {
        return Err(format!("the browser could not load {url}: {problem}"));
    }
    session.url = url.to_string();
    // Not an event subscription: `Page.navigate` resolves before subresources are done, so the
    // page is polled until it says it is complete. A page that never gets there is not an error
    // — it may be waiting on something only a later call can supply.
    let stop = Instant::now() + Duration::from_millis(LOAD_TIMEOUT_MS);
    while Instant::now() < stop {
        match session.evaluate("document.readyState", call_deadline()) {
            Ok(result) => {
                if describe_value(result.get("result")).contains("complete") {
                    break;
                }
            }
            Err(Failure::Transport(message)) => return Err(message),
            Err(Failure::Protocol(_)) => break,
        }
        std::thread::sleep(POLL_INTERVAL);
    }
    Ok(())
}

// --- pure helpers --------------------------------------------------------------------------

/// The port and browser endpoint from a `DevToolsActivePort` file.
///
/// The file is two lines: the port Chromium picked, then the browser's WebSocket path.
fn parse_devtools_port(text: &str) -> Option<(u16, String)> {
    let mut lines = text.lines().map(str::trim).filter(|line| !line.is_empty());
    let port = lines.next()?.parse::<u16>().ok()?;
    let path = lines.next().unwrap_or_default().to_string();
    Some((port, path))
}

/// `("127.0.0.1", 9222, "/devtools/page/…")` from `ws://127.0.0.1:9222/devtools/page/…`.
fn parse_ws_url(url: &str) -> Option<(String, u16, String)> {
    let rest = url
        .strip_prefix("ws://")
        .or_else(|| url.strip_prefix("WS://"))?;
    let (authority, path) = match rest.find('/') {
        Some(at) => (&rest[..at], &rest[at..]),
        None => (rest, "/"),
    };
    if authority.is_empty() {
        return None;
    }
    let (host, port) = match authority.rsplit_once(':') {
        Some((host, port)) => (host, port.parse::<u16>().ok()?),
        None => (authority, 80),
    };
    if host.is_empty() {
        return None;
    }
    Some((host.to_string(), port, path.to_string()))
}

/// Whether `raw` already holds a whole HTTP reply.
///
/// Chromium does not close the connection after answering, so the reader has to decide for
/// itself when to stop: `Content-Length` bytes past the header, a terminating chunk, or — with
/// neither header — the peer hanging up.
fn response_is_complete(raw: &[u8]) -> bool {
    let Ok(text) = std::str::from_utf8(raw) else {
        return false;
    };
    let Some(split) = text.find("\r\n\r\n") else {
        return false;
    };
    let head = &text[..split];
    let body = &text[split + 4..];
    if head
        .to_ascii_lowercase()
        .contains("transfer-encoding: chunked")
    {
        return body.ends_with("0\r\n\r\n");
    }
    match content_length(head) {
        Some(length) => body.len() >= length,
        None => false,
    }
}

/// The `Content-Length` of a header block, if it names one.
fn content_length(head: &str) -> Option<usize> {
    for line in head.lines().skip(1) {
        let Some((name, value)) = line.split_once(':') else {
            continue;
        };
        if name.trim().eq_ignore_ascii_case("content-length") {
            return value.trim().parse::<usize>().ok();
        }
    }
    None
}

/// The body of an HTTP/1.1 reply, or an error naming the status line.
///
/// Chromium answers these requests with `Content-Length`; chunked framing is handled anyway
/// because it costs a dozen lines and a silent truncation would be hard to diagnose.
fn parse_http_response(raw: &str) -> Result<String, String> {
    let split = raw
        .find("\r\n\r\n")
        .ok_or_else(|| "the debugger's reply had no header terminator".to_string())?;
    let (head, body) = (&raw[..split], &raw[split + 4..]);
    let status = head.lines().next().unwrap_or_default();
    if !status.contains(" 200") {
        return Err(format!("the debugger refused the request: {status}"));
    }
    if head
        .to_ascii_lowercase()
        .contains("transfer-encoding: chunked")
    {
        return dechunk(body);
    }
    Ok(body.to_string())
}

/// Decode HTTP chunked framing.
fn dechunk(body: &str) -> Result<String, String> {
    let mut out = String::with_capacity(body.len());
    let mut cursor = 0usize;
    while cursor < body.len() {
        let line_end = body[cursor..]
            .find("\r\n")
            .ok_or_else(|| "a chunk header was unterminated".to_string())?
            + cursor;
        let header = body[cursor..line_end].trim();
        let size_text = header.split(';').next().unwrap_or(header).trim();
        let size = usize::from_str_radix(size_text, 16)
            .map_err(|_| format!("`{size_text}` is not a chunk size"))?;
        if size == 0 {
            return Ok(out);
        }
        let start = line_end + 2;
        let end = start
            .checked_add(size)
            .filter(|end| *end <= body.len())
            .ok_or_else(|| "a chunk ran past the end of the reply".to_string())?;
        out.push_str(&body[start..end]);
        cursor = end + 2;
    }
    Ok(out)
}

/// A JavaScript string literal for `text`. JSON escaping is valid JavaScript escaping.
fn js_string(text: &str) -> String {
    serde_json::to_string(text).unwrap_or_else(|_| "\"\"".to_string())
}

/// JavaScript that measures the first element matching `selector`.
///
/// The reply is `null` when nothing matches and otherwise the centre of the element's viewport
/// box plus the box's own size: enough to aim a mouse event, and enough to notice an element
/// that is present but has no size to aim at.
pub fn js_element_center(selector: &str) -> String {
    let sel = js_string(selector);
    format!(
        "(() => {{ const el = document.querySelector({sel}); if (!el) return null; \
             const r = el.getBoundingClientRect(); \
             return {{x: r.left + r.width/2, y: r.top + r.height/2, w: r.width, h: r.height}}; }})()"
    )
}

/// JavaScript that scrolls the first element matching `selector` to the middle of the viewport.
///
/// Reports whether the element existed at all, so a vanished target is an error naming the
/// selector rather than a mouse event at the origin.
pub fn js_scroll_into_view(selector: &str) -> String {
    let sel = js_string(selector);
    format!(
        "(() => {{ const el = document.querySelector({sel}); if (!el) return false; \
             el.scrollIntoView({{block: 'center', inline: 'center'}}); return true; }})()"
    )
}

/// JavaScript that focuses the first element matching `selector`, reporting whether it existed.
pub fn js_focus(selector: &str) -> String {
    let sel = js_string(selector);
    format!(
        "(() => {{ const el = document.querySelector({sel}); if (!el) return false; \
             el.focus(); return true; }})()"
    )
}

/// JavaScript that empties an `<input>`/`<textarea>` before trusted typing replaces its value.
///
/// Only those two elements have a `value` worth clearing. A `contentEditable` is left alone:
/// emptying its `textContent` would drop the caret the next keystrokes are inserted at, which is
/// precisely what typing into one depends on.
pub fn js_clear_value(selector: &str) -> String {
    let sel = js_string(selector);
    format!(
        "(() => {{ const el = document.querySelector({sel}); if (!el) return false; \
             const tag = el.tagName; \
             if (tag === 'INPUT' || tag === 'TEXTAREA') {{ el.value = ''; }} \
             return true; }})()"
    )
}

/// JavaScript that returns the page's visible text.
pub fn js_text() -> String {
    "(() => { const body = document.body; return body ? body.innerText : ''; })()".to_string()
}

/// The `value` inside a `Runtime.evaluate` reply, ignoring its `type` wrapper.
fn reply_value(reply: &Value) -> Option<&Value> {
    reply.get("result")?.get("value")
}

/// An element's viewport box, as [`js_element_center`] reported it.
///
/// The centre is what a mouse event needs; the size is kept because an element that measures as
/// a point — `display: none`, or inside a hidden ancestor — cannot be clicked meaningfully.
#[derive(Debug, Clone, Copy, PartialEq)]
struct Rect {
    /// Centre, in CSS pixels from the viewport's left edge.
    x: f64,
    /// Centre, in CSS pixels from the viewport's top edge.
    y: f64,
    w: f64,
    h: f64,
}

impl Rect {
    /// Whether the element occupies any space at all.
    fn has_area(&self) -> bool {
        self.w > 0.0 && self.h > 0.0
    }
}

/// The box in a `Runtime.evaluate` reply, or `None` when the script returned `null`.
///
/// `None` covers both halves of "no element": the reply carries `value: null`, and `value` may
/// be absent entirely when the script returned something unserializable.
fn parse_rect(result: Option<&Value>) -> Option<Rect> {
    let value = result?.get("value")?;
    let field = |name: &str| value.get(name).and_then(Value::as_f64);
    Some(Rect {
        x: field("x")?,
        y: field("y")?,
        w: field("w")?,
        h: field("h")?,
    })
}

/// The parameters of a `mousePressed`/`mouseReleased` event at `(x, y)`.
///
/// A left button and a click count of one are what turn the pair into a click; the browser fills
/// in everything else — the pointer type, the target, the event's own timestamps — itself.
fn mouse_params(kind: &str, x: f64, y: f64) -> Value {
    json!({"type": kind, "x": x, "y": y, "button": "left", "clickCount": 1})
}

/// The parameters of the wheel event that carries a scroll of `delta_x`/`delta_y`.
fn wheel_params(delta_x: i64, delta_y: i64) -> Value {
    json!({
        "type": "mouseWheel",
        "x": 0,
        "y": 0,
        "deltaX": delta_x,
        "deltaY": delta_y
    })
}

/// CDP's modifier bit for Alt/Option.
const MOD_ALT: i64 = 1;
/// CDP's modifier bit for Control.
const MOD_CTRL: i64 = 2;
/// CDP's modifier bit for Meta/Command.
const MOD_META: i64 = 4;
/// CDP's modifier bit for Shift.
const MOD_SHIFT: i64 = 8;

/// One key press, parsed from a combination like `ctrl+l` or a name like `Enter`.
#[derive(Debug, Clone, PartialEq)]
struct KeyStroke {
    /// `KeyboardEvent.key`.
    key: String,
    /// `KeyboardEvent.code`; empty when the name has no obvious code.
    code: String,
    /// The modifiers bitmask CDP expects: alt 1, ctrl 2, meta 4, shift 8.
    modifiers: i64,
    /// What the press would insert, if anything. `None` for Tab, the arrows and modified keys.
    text: Option<String>,
    /// The Windows virtual-key code, which is what makes Enter, Tab and the arrows *act*
    /// rather than merely be observed.
    vk: Option<i64>,
}

impl KeyStroke {
    /// The `Input.dispatchKeyEvent` parameters that press this key.
    fn down_params(&self) -> Value {
        let mut params = self.bare_params("keyDown");
        if let Some(text) = &self.text {
            params["text"] = json!(text);
            params["unmodifiedText"] = json!(text);
        }
        if let Some(vk) = self.vk {
            params["windowsVirtualKeyCode"] = json!(vk);
            params["nativeVirtualKeyCode"] = json!(vk);
        }
        params
    }

    /// The `Input.dispatchKeyEvent` parameters that release it.
    fn up_params(&self) -> Value {
        let mut params = self.bare_params("keyUp");
        if let Some(vk) = self.vk {
            params["windowsVirtualKeyCode"] = json!(vk);
            params["nativeVirtualKeyCode"] = json!(vk);
        }
        params
    }

    fn bare_params(&self, kind: &str) -> Value {
        json!({
            "type": kind,
            "key": self.key.as_str(),
            "code": self.code.as_str(),
            "modifiers": self.modifiers
        })
    }
}

/// Parse `combo` into one key press: `ctrl+l`, `shift+Tab`, `Meta+ArrowDown`, `Enter`, `a`.
///
/// `None` when the combination is empty or names a modifier that is not one, so `hyper+a` is
/// refused rather than sent as a key called `hyper`. A trailing `+` names the plus key itself,
/// which is the one key whose name collides with the separator — but only when the combination
/// says so twice (`ctrl++`) or is nothing but `+`, because `ctrl+` on its own is a typo rather
/// than a keystroke.
fn parse_key_combo(combo: &str) -> Option<KeyStroke> {
    let combo = combo.trim();
    if combo.is_empty() {
        return None;
    }
    let (modifier_part, key_part) = match combo.rsplit_once('+') {
        Some((head, "")) if head.is_empty() || head.ends_with('+') => {
            (head.strip_suffix('+').unwrap_or(""), "+")
        }
        Some((_, "")) => return None,
        Some((head, tail)) => (head, tail),
        None => ("", combo),
    };
    let mut modifiers = 0i64;
    for token in modifier_part.split('+').filter(|part| !part.is_empty()) {
        modifiers |= modifier_bit(token)?;
    }
    Some(key_stroke(key_part, modifiers))
}

/// The CDP bit for one modifier name, or `None` when it is not a modifier.
fn modifier_bit(token: &str) -> Option<i64> {
    match token.trim().to_ascii_lowercase().as_str() {
        "alt" | "option" => Some(MOD_ALT),
        "ctrl" | "control" => Some(MOD_CTRL),
        "meta" | "cmd" | "command" | "super" => Some(MOD_META),
        "shift" => Some(MOD_SHIFT),
        _ => None,
    }
}

/// The key called `name`, held with `modifiers`.
///
/// A name in [`named_key`] wins over the single-character reading, so `Tab` is a tab key and not
/// the letter T; anything else of one character is that character, and any other name is passed
/// through as a key of that name rather than refused.
fn key_stroke(name: &str, modifiers: i64) -> KeyStroke {
    let lower = name.to_ascii_lowercase();
    if let Some((key, code, vk, text)) = named_key(&lower) {
        return KeyStroke {
            key: key.to_string(),
            code: code.to_string(),
            modifiers,
            text: text.map(str::to_string),
            vk,
        };
    }
    let mut chars = name.chars();
    if let (Some(ch), None) = (chars.next(), chars.next()) {
        // Shift turns a letter into its upper-case self, which is what the page would see.
        let ch = if modifiers & MOD_SHIFT != 0 {
            ch.to_ascii_uppercase()
        } else {
            ch
        };
        // A chord is a shortcut, not text: `ctrl+l` must not insert an `l` into the page.
        let chorded = modifiers & (MOD_ALT | MOD_CTRL | MOD_META) != 0;
        return KeyStroke {
            key: ch.to_string(),
            code: char_code(ch),
            modifiers,
            text: (!chorded).then(|| ch.to_string()),
            vk: None,
        };
    }
    KeyStroke {
        key: name.to_string(),
        code: String::new(),
        modifiers,
        text: None,
        vk: None,
    }
}

/// The keys with a name of their own: `(key, code, virtual-key code, text)`.
///
/// The virtual-key code is what makes the browser act on the press rather than merely deliver
/// it — a `Tab` without one moves nothing. `None` stands for "no text is inserted": Enter gets
/// `\r` because that is what submits a form and confirms a dialog.
fn named_key(
    lower: &str,
) -> Option<(
    &'static str,
    &'static str,
    Option<i64>,
    Option<&'static str>,
)> {
    let entry = match lower {
        "enter" | "return" => ("Enter", "Enter", Some(13), Some("\r")),
        "tab" => ("Tab", "Tab", Some(9), None),
        "escape" | "esc" => ("Escape", "Escape", Some(27), None),
        "backspace" => ("Backspace", "Backspace", Some(8), None),
        "delete" | "del" => ("Delete", "Delete", Some(46), None),
        "space" | "spacebar" => (" ", "Space", Some(32), Some(" ")),
        "arrowup" | "up" => ("ArrowUp", "ArrowUp", Some(38), None),
        "arrowdown" | "down" => ("ArrowDown", "ArrowDown", Some(40), None),
        "arrowleft" | "left" => ("ArrowLeft", "ArrowLeft", Some(37), None),
        "arrowright" | "right" => ("ArrowRight", "ArrowRight", Some(39), None),
        "home" => ("Home", "Home", Some(36), None),
        "end" => ("End", "End", Some(35), None),
        "pageup" => ("PageUp", "PageUp", Some(33), None),
        "pagedown" => ("PageDown", "PageDown", Some(34), None),
        _ => return None,
    };
    Some(entry)
}

/// `KeyboardEvent.code` for a printable character: `l` is `KeyL`, `7` is `Digit7`.
fn char_code(ch: char) -> String {
    if ch.is_ascii_alphabetic() {
        format!("Key{}", ch.to_ascii_uppercase())
    } else if ch.is_ascii_digit() {
        format!("Digit{ch}")
    } else {
        String::new()
    }
}

/// A `Runtime.evaluate` reply's value as text: strings verbatim, everything else as JSON.
fn describe_value(result: Option<&Value>) -> String {
    let Some(result) = result else {
        return "undefined".to_string();
    };
    match result.get("value") {
        Some(Value::String(text)) => text.clone(),
        Some(value) => serde_json::to_string(value).unwrap_or_else(|_| value.to_string()),
        None => match result.get("type").and_then(Value::as_str) {
            Some(kind) if kind != "undefined" => format!("<{kind} with no serializable value>"),
            _ => "undefined".to_string(),
        },
    }
}

/// `code: message` from a CDP `error` object.
fn describe_cdp_error(error: &Value) -> String {
    let code = error.get("code").and_then(Value::as_i64);
    let message = error
        .get("message")
        .and_then(Value::as_str)
        .unwrap_or("no message");
    match code {
        Some(code) => format!("{code}: {message}"),
        None => message.to_string(),
    }
}

/// The most specific explanation a `Runtime.evaluate` exception carries.
fn describe_exception(result: &Value) -> String {
    let details = result.get("exceptionDetails").unwrap_or(&Value::Null);
    if let Some(description) = details
        .get("exception")
        .and_then(|e| e.get("description"))
        .and_then(Value::as_str)
    {
        return first_line(description);
    }
    if let Some(text) = details.get("text").and_then(Value::as_str) {
        return first_line(text);
    }
    "the script failed without an explanation".to_string()
}

fn first_line(text: &str) -> String {
    text.lines().next().unwrap_or(text).trim().to_string()
}

#[cfg(test)]
mod tests {
    use super::*;

    // --- identity -------------------------------------------------------------------------

    #[test]
    fn session_ids_are_numbered_from_one() {
        assert_eq!(session_id(1), "browser-1");
        assert_eq!(session_id(42), "browser-42");
    }

    // --- the browser's handshake files ----------------------------------------------------

    #[test]
    fn the_active_port_file_is_two_lines() {
        let found = parse_devtools_port("9222\n/devtools/browser/6f1e-aa\n");
        assert_eq!(found, Some((9222, "/devtools/browser/6f1e-aa".to_string())));
    }

    #[test]
    fn the_active_port_file_may_arrive_half_written() {
        // The port alone is enough to talk HTTP to the browser; the endpoint line is a nicety.
        assert_eq!(parse_devtools_port("9222\n"), Some((9222, String::new())));
        assert_eq!(parse_devtools_port("9222"), Some((9222, String::new())));
    }

    #[test]
    fn garbage_in_the_active_port_file_is_not_a_port() {
        assert_eq!(parse_devtools_port(""), None);
        assert_eq!(parse_devtools_port("\n\n"), None);
        assert_eq!(parse_devtools_port("not-a-port\n/devtools/browser/x"), None);
        assert_eq!(parse_devtools_port("70000\n/x"), None);
    }

    #[test]
    fn ws_urls_are_split_into_host_port_and_path() {
        let parsed = parse_ws_url("ws://127.0.0.1:9222/devtools/page/ABC-1");
        assert_eq!(
            parsed,
            Some((
                "127.0.0.1".to_string(),
                9222,
                "/devtools/page/ABC-1".to_string()
            ))
        );
    }

    #[test]
    fn a_ws_url_without_a_port_or_path_still_parses() {
        assert_eq!(
            parse_ws_url("ws://127.0.0.1"),
            Some(("127.0.0.1".to_string(), 80, "/".to_string()))
        );
    }

    #[test]
    fn only_plain_ws_urls_parse() {
        assert_eq!(parse_ws_url("wss://127.0.0.1:9222/x"), None);
        assert_eq!(parse_ws_url("http://127.0.0.1:9222/x"), None);
        assert_eq!(parse_ws_url("ws://"), None);
        assert_eq!(parse_ws_url("ws://:9222/x"), None);
        assert_eq!(parse_ws_url("ws://127.0.0.1:port/x"), None);
    }

    // --- the debugger's HTTP replies -------------------------------------------------------

    #[test]
    fn an_http_reply_yields_its_body() {
        let raw = "HTTP/1.1 200 OK\r\nContent-Length: 2\r\n\r\n[]";
        assert_eq!(parse_http_response(raw).unwrap(), "[]");
    }

    #[test]
    fn a_non_200_reply_is_an_error_naming_the_status() {
        let raw = "HTTP/1.1 404 Not Found\r\nContent-Length: 0\r\n\r\n";
        let err = parse_http_response(raw).unwrap_err();
        assert!(err.contains("404"), "{err}");
    }

    #[test]
    fn a_reply_without_a_header_terminator_is_an_error() {
        assert!(parse_http_response("HTTP/1.1 200 OK").is_err());
    }

    #[test]
    fn a_chunked_reply_is_decoded() {
        let raw = "HTTP/1.1 200 OK\r\nTransfer-Encoding: chunked\r\n\r\n\
                       4\r\nWiki\r\n5\r\npedia\r\n0\r\n\r\n";
        assert_eq!(parse_http_response(raw).unwrap(), "Wikipedia");
    }

    #[test]
    fn chunk_extensions_and_short_chunks_are_handled() {
        assert_eq!(dechunk("3;ext=1\r\nabc\r\n0\r\n\r\n").unwrap(), "abc");
        let err = dechunk("14\r\nabc\r\n0\r\n\r\n").unwrap_err();
        assert!(err.contains("past the end"), "{err}");
        let err = dechunk("zz\r\nabc\r\n").unwrap_err();
        assert!(err.contains("not a chunk size"), "{err}");
    }

    #[test]
    fn a_length_only_reply_is_complete_exactly_at_its_length() {
        // Chromium leaves the socket open after answering, so `read_to_end` would hang: the
        // reader has to stop at `Content-Length`.
        assert_eq!(
            content_length("HTTP/1.1 200 OK\r\nContent-Length:3112"),
            Some(3112)
        );
        assert_eq!(
            content_length("HTTP/1.1 200 OK\r\nContent-Length: 12"),
            Some(12)
        );
        assert_eq!(content_length("HTTP/1.1 200 OK\r\n"), None);
        let head = "HTTP/1.1 200 OK\r\nContent-Length:2\r\n\r\n";
        assert!(!response_is_complete(head.as_bytes()));
        assert!(response_is_complete(format!("{head}[]").as_bytes()));
        assert!(response_is_complete(format!("{head}[]trailing").as_bytes()));
    }

    #[test]
    fn a_reply_without_its_headers_yet_is_not_complete() {
        assert!(!response_is_complete(b"HTTP/1.1 200 OK\r\n"));
        assert!(!response_is_complete(b""));
        assert!(!response_is_complete(&[0xff, 0xfe]));
    }

    #[test]
    fn a_chunked_reply_is_complete_at_its_terminating_chunk() {
        let start = "HTTP/1.1 200 OK\r\nTransfer-Encoding: chunked\r\n\r\n";
        assert!(!response_is_complete(format!("{start}4\r\nWi").as_bytes()));
        assert!(response_is_complete(
            format!("{start}4\r\nWiki\r\n0\r\n\r\n").as_bytes()
        ));
    }

    // --- generated JavaScript -------------------------------------------------------------

    #[test]
    fn the_measurement_script_reads_the_box_of_the_matching_element() {
        let js = js_element_center("#go");
        assert!(js.contains(r##"document.querySelector("#go")"##), "{js}");
        assert!(js.contains("getBoundingClientRect()"), "{js}");
        // The centre, not the corner: a click at the top-left of a button can miss it.
        assert!(js.contains("r.left + r.width/2"), "{js}");
        assert!(js.contains("r.top + r.height/2"), "{js}");
        // A missing element is `null`, which is what the caller turns into an error.
        assert!(js.contains("if (!el) return null;"), "{js}");
    }

    #[test]
    fn scrolling_into_view_and_focusing_report_whether_the_element_existed() {
        let scroll = js_scroll_into_view("#go");
        assert!(scroll.contains("el.scrollIntoView("), "{scroll}");
        assert!(scroll.contains("block: 'center'"), "{scroll}");
        assert!(scroll.contains("if (!el) return false;"), "{scroll}");
        assert!(scroll.contains("return true;"), "{scroll}");
        let focus = js_focus("#go");
        assert!(focus.contains("el.focus()"), "{focus}");
        assert!(focus.contains("if (!el) return false;"), "{focus}");
    }

    #[test]
    fn clearing_empties_a_value_field_and_leaves_a_content_editable_alone() {
        let js = js_clear_value("#q");
        assert!(js.contains("el.value = ''"), "{js}");
        assert!(js.contains("tag === 'INPUT'"), "{js}");
        assert!(js.contains("tag === 'TEXTAREA'"), "{js}");
        // Nothing may touch `innerText`/`textContent`: that would destroy a caret.
        assert!(!js.contains("textContent"), "{js}");
        assert!(!js.contains("innerText"), "{js}");
    }

    #[test]
    fn no_generated_script_synthesises_a_click_or_dispatches_an_event() {
        // The whole point of the trusted path: script measures, scrolls and focuses, and never
        // fakes an event a page could tell apart from a real one.
        for js in [
            js_element_center("#go"),
            js_scroll_into_view("#go"),
            js_focus("#go"),
            js_clear_value("#go"),
        ] {
            assert!(!js.contains(".click()"), "{js}");
            assert!(!js.contains("dispatchEvent"), "{js}");
            assert!(!js.contains("new Event("), "{js}");
        }
    }

    #[test]
    fn a_selector_cannot_break_out_of_its_string_literal() {
        // A selector is attacker-influenced text; it must arrive as one JS string, not as code.
        let hostile = r#"a[title="x"]'); alert(1); //"#;
        for js in [
            js_element_center(hostile),
            js_scroll_into_view(hostile),
            js_focus(hostile),
            js_clear_value(hostile),
        ] {
            assert!(
                js.contains(r#"document.querySelector("a[title=\"x\"]'); alert(1); //")"#),
                "{js}"
            );
            // The would-be code is only ever inside a quoted literal: every occurrence escaped.
            assert_eq!(js.matches(r#"\""#).count() % 2, 0, "{js}");
            assert!(!js.contains("\"x\"]'); alert"), "{js}");
        }
    }

    #[test]
    fn a_selector_with_a_newline_or_a_backslash_stays_one_escaped_literal() {
        // JSON escaping is JavaScript escaping for exactly these characters, which is why the
        // selector is injected through `serde_json` rather than pasted in.
        let js = js_element_center("a[href=\"a\nb\"]\\");
        assert!(
            js.contains(r#"document.querySelector("a[href=\"a\nb\"]\\")"#),
            "{js}"
        );
        assert!(
            !js.contains("a\nb"),
            "the newline must not be a real one: {js}"
        );
        assert_eq!(js_string("a\nb"), r#""a\nb""#);
        assert_eq!(js_string(r"a\b"), r#""a\\b""#);
        assert_eq!(js_string("a\tb"), r#""a\tb""#);
    }

    // --- the box a measurement returns ----------------------------------------------------

    #[test]
    fn a_measured_box_is_read_out_of_the_reply() {
        let reply = json!({"result": {"type": "object",
                "value": {"x": 12.5, "y": 40.0, "w": 100.0, "h": 20.0}}});
        let rect = parse_rect(reply.get("result")).expect("a box");
        assert_eq!(
            rect,
            Rect {
                x: 12.5,
                y: 40.0,
                w: 100.0,
                h: 20.0
            }
        );
        assert!(rect.has_area());
    }

    #[test]
    fn a_null_reply_means_nothing_matched() {
        let reply = json!({"result": {"type": "object", "subtype": "null", "value": null}});
        assert_eq!(parse_rect(reply.get("result")), None);
        assert_eq!(parse_rect(None), None);
        // A reply with no serializable value is equally "no element".
        assert_eq!(
            parse_rect(Some(&json!({"type": "undefined"}))),
            None,
            "a missing value is not a box"
        );
    }

    #[test]
    fn a_box_with_a_missing_or_non_numeric_field_is_not_a_box() {
        for value in [
            json!({"y": 1.0, "w": 2.0, "h": 3.0}),
            json!({"x": 0.0, "w": 2.0, "h": 3.0}),
            json!({"x": 0.0, "y": 1.0, "h": 3.0}),
            json!({"x": 0.0, "y": 1.0, "w": 2.0}),
            json!({"x": "0", "y": 1.0, "w": 2.0, "h": 3.0}),
            json!("not a box"),
        ] {
            assert_eq!(parse_rect(Some(&json!({"value": value}))), None);
        }
    }

    #[test]
    fn an_element_with_no_area_is_recognised() {
        let hidden = Rect {
            x: 0.0,
            y: 0.0,
            w: 0.0,
            h: 0.0,
        };
        assert!(!hidden.has_area());
        assert!(
            !Rect {
                x: 0.0,
                y: 0.0,
                w: 100.0,
                h: 0.0
            }
            .has_area()
        );
        assert!(
            !Rect {
                x: 0.0,
                y: 0.0,
                w: 0.0,
                h: 20.0
            }
            .has_area()
        );
        assert!(
            Rect {
                x: 0.0,
                y: 0.0,
                w: 0.5,
                h: 0.5
            }
            .has_area()
        );
    }

    // --- the input events built from a measurement ----------------------------------------

    #[test]
    fn a_click_is_a_left_press_and_release_at_the_same_point() {
        let down = mouse_params("mousePressed", 100.0, 200.0);
        assert_eq!(down["type"], json!("mousePressed"));
        assert_eq!(down["x"], json!(100.0));
        assert_eq!(down["y"], json!(200.0));
        assert_eq!(down["button"], json!("left"));
        assert_eq!(down["clickCount"], json!(1));
        let up = mouse_params("mouseReleased", 100.0, 200.0);
        assert_eq!(up["type"], json!("mouseReleased"));
        assert_eq!(up["button"], json!("left"));
        // The pair must land on the same pixel or the browser sees a drag, not a click.
        assert_eq!(down["x"], up["x"]);
        assert_eq!(down["y"], up["y"]);
    }

    #[test]
    fn a_scroll_is_a_wheel_event_carrying_both_deltas() {
        let wheel = wheel_params(0, 400);
        assert_eq!(wheel["type"], json!("mouseWheel"));
        assert_eq!(wheel["deltaX"], json!(0));
        assert_eq!(wheel["deltaY"], json!(400));
        assert_eq!(wheel["x"], json!(0));
        assert_eq!(wheel["y"], json!(0));
    }

    #[test]
    fn a_negative_scroll_offset_survives_into_the_event() {
        let wheel = wheel_params(-50, -25);
        assert_eq!(wheel["deltaX"], json!(-50));
        assert_eq!(wheel["deltaY"], json!(-25));
    }

    // --- the key-combination parser -------------------------------------------------------

    #[test]
    fn a_bare_key_name_parses_to_that_key() {
        let enter = parse_key_combo("Enter").expect("Enter parses");
        assert_eq!(enter.key, "Enter");
        assert_eq!(enter.code, "Enter");
        assert_eq!(enter.modifiers, 0);
        assert_eq!(enter.vk, Some(13));
        // Enter has to carry `\r`: that is what submits a form.
        assert_eq!(enter.text.as_deref(), Some("\r"));
    }

    #[test]
    fn key_names_are_case_insensitive_and_may_be_aliases() {
        assert_eq!(parse_key_combo("tab").unwrap().key, "Tab");
        assert_eq!(parse_key_combo("TAB").unwrap().key, "Tab");
        assert_eq!(parse_key_combo("esc").unwrap().key, "Escape");
        assert_eq!(parse_key_combo("return").unwrap().key, "Enter");
        assert_eq!(parse_key_combo("down").unwrap().key, "ArrowDown");
        assert_eq!(parse_key_combo("ArrowDown").unwrap().code, "ArrowDown");
        assert_eq!(parse_key_combo("ArrowDown").unwrap().vk, Some(40));
    }

    #[test]
    fn modifiers_are_packed_into_the_bitmask_the_protocol_expects() {
        // Alt 1, Ctrl 2, Meta 4, Shift 8.
        assert_eq!(parse_key_combo("ctrl+l").unwrap().modifiers, MOD_CTRL);
        assert_eq!(parse_key_combo("ctrl+l").unwrap().modifiers, 2);
        assert_eq!(parse_key_combo("shift+tab").unwrap().modifiers, 8);
        assert_eq!(parse_key_combo("alt+x").unwrap().modifiers, 1);
        assert_eq!(parse_key_combo("Meta+ArrowDown").unwrap().modifiers, 4);
        assert_eq!(
            parse_key_combo("ctrl+shift+l").unwrap().modifiers,
            MOD_CTRL | MOD_SHIFT
        );
        assert_eq!(
            parse_key_combo("ctrl+alt+meta+shift+l").unwrap().modifiers,
            15
        );
        // Spelling is not significant, and `cmd`/`super`/`option` are the same bits.
        assert_eq!(parse_key_combo("Control+L").unwrap().modifiers, MOD_CTRL);
        assert_eq!(parse_key_combo("cmd+l").unwrap().modifiers, MOD_META);
        assert_eq!(parse_key_combo("option+l").unwrap().modifiers, MOD_ALT);
    }

    #[test]
    fn a_letter_becomes_its_key_code_and_a_chord_inserts_no_text() {
        let plain = parse_key_combo("l").unwrap();
        assert_eq!(plain.key, "l");
        assert_eq!(plain.code, "KeyL");
        assert_eq!(plain.text.as_deref(), Some("l"));
        // `ctrl+l` focuses the address bar in a browser; it must not type an `l` as well.
        let chorded = parse_key_combo("ctrl+l").unwrap();
        assert_eq!(chorded.key, "l");
        assert_eq!(chorded.code, "KeyL");
        assert_eq!(chorded.text, None);
        // Shift upper-cases the letter, as the page would see it.
        let shifted = parse_key_combo("shift+a").unwrap();
        assert_eq!(shifted.key, "A");
        assert_eq!(shifted.code, "KeyA");
        assert_eq!(parse_key_combo("7").unwrap().code, "Digit7");
        assert_eq!(parse_key_combo("/").unwrap().code, "");
    }

    #[test]
    fn an_unknown_key_name_is_still_sent_rather_than_refused() {
        let stroke = parse_key_combo("F5").expect("a best-effort key");
        assert_eq!(stroke.key, "F5");
        assert_eq!(stroke.vk, None);
    }

    #[test]
    fn a_combo_that_makes_no_sense_is_refused() {
        assert_eq!(parse_key_combo(""), None);
        assert_eq!(parse_key_combo("   "), None);
        // A modifier nobody implements is an error, not a key called `hyper`.
        assert_eq!(parse_key_combo("hyper+a"), None);
        assert_eq!(parse_key_combo("ctrl+"), None);
    }

    #[test]
    fn a_trailing_plus_names_the_plus_key() {
        let stroke = parse_key_combo("ctrl++").expect("the plus key");
        assert_eq!(stroke.key, "+");
        assert_eq!(stroke.modifiers, MOD_CTRL);
        assert_eq!(parse_key_combo("+").unwrap().key, "+");
        assert_eq!(parse_key_combo("+").unwrap().modifiers, 0);
        // One `+` after a modifier is a dangling separator, not a keystroke: typing `ctrl+`
        // must not press ctrl plus by accident.
        assert_eq!(parse_key_combo("ctrl+"), None);
    }

    #[test]
    fn a_key_press_is_a_key_down_and_a_key_up_carrying_the_same_shape() {
        let stroke = parse_key_combo("ctrl+shift+k").unwrap();
        let down = stroke.down_params();
        assert_eq!(down["type"], json!("keyDown"));
        // Shift is held, so the page sees `K` — and the code names the physical key.
        assert_eq!(down["key"], json!("K"));
        assert_eq!(down["code"], json!("KeyK"));
        assert_eq!(down["modifiers"], json!(10));
        let up = stroke.up_params();
        assert_eq!(up["type"], json!("keyUp"));
        assert_eq!(up["key"], down["key"]);
        assert_eq!(up["modifiers"], down["modifiers"]);
        // A chord releases without inserting anything.
        assert!(down.get("text").is_none(), "{down}");

        // A named key carries the virtual-key code that makes it act.
        let tab = parse_key_combo("Tab").unwrap();
        assert_eq!(tab.down_params()["windowsVirtualKeyCode"], json!(9));
        assert_eq!(tab.up_params()["windowsVirtualKeyCode"], json!(9));
        assert!(
            tab.down_params().get("text").is_none(),
            "Tab inserts nothing"
        );
    }

    #[test]
    fn an_unmodified_character_carries_its_text() {
        let down = parse_key_combo("a").unwrap().down_params();
        assert_eq!(down["text"], json!("a"));
        assert_eq!(down["unmodifiedText"], json!("a"));
    }

    #[test]
    fn key_is_refused_before_the_browser_is_touched_when_the_combo_is_unusable() {
        let err = key("browser-does-not-exist", "  ").unwrap_err();
        assert!(err.contains("`combo` must not be empty"), "{err}");
        let err = key("browser-does-not-exist", "hyper+a").unwrap_err();
        assert!(err.contains("cannot parse key combo"), "{err}");
    }

    #[test]
    fn text_reads_the_body_and_survives_a_missing_one() {
        let js = js_text();
        assert!(js.contains("document.body"), "{js}");
        assert!(js.contains("body.innerText"), "{js}");
        assert!(js.contains("return body ?"), "{js}");
    }

    // --- reading replies ------------------------------------------------------------------

    #[test]
    fn a_string_result_comes_back_verbatim() {
        let result = json!({"type": "string", "value": "hello"});
        assert_eq!(describe_value(Some(&result)), "hello");
    }

    #[test]
    fn a_non_string_result_comes_back_as_json() {
        assert_eq!(
            describe_value(Some(&json!({"type": "number", "value": 7}))),
            "7"
        );
        assert_eq!(
            describe_value(Some(&json!({"type": "object", "value": {"a": 1}}))),
            r#"{"a":1}"#
        );
    }

    #[test]
    fn an_undefined_result_is_named() {
        assert_eq!(describe_value(None), "undefined");
        assert_eq!(
            describe_value(Some(&json!({"type": "undefined"}))),
            "undefined"
        );
        assert_eq!(
            describe_value(Some(&json!({"type": "function", "className": "Function"}))),
            "<function with no serializable value>"
        );
    }

    #[test]
    fn a_cdp_error_carries_its_code() {
        let error = json!({"code": -32602, "message": "Invalid parameters"});
        assert_eq!(describe_cdp_error(&error), "-32602: Invalid parameters");
        assert_eq!(describe_cdp_error(&json!({"message": "nope"})), "nope");
        assert_eq!(describe_cdp_error(&json!({"code": -1})), "-1: no message");
    }

    #[test]
    fn an_exception_prefers_its_description_over_its_headline() {
        let result = json!({"exceptionDetails": {
            "text": "Uncaught",
            "exception": {"description": "TypeError: null is not an object\n    at <anonymous>:1:1"}
        }});
        assert_eq!(
            describe_exception(&result),
            "TypeError: null is not an object"
        );
        assert_eq!(
            describe_exception(&json!({"exceptionDetails": {"text": "Uncaught"}})),
            "Uncaught"
        );
        assert_eq!(
            describe_exception(&json!({"exceptionDetails": {}})),
            "the script failed without an explanation"
        );
    }

    // --- refusals that need no browser ----------------------------------------------------

    #[test]
    fn an_unknown_session_is_named_in_the_error() {
        let err = eval("browser-does-not-exist", "1 + 1").unwrap_err();
        assert!(err.contains("unknown browser session"), "{err}");
        assert!(err.contains("browser-does-not-exist"), "{err}");
    }

    #[test]
    fn an_empty_script_or_selector_is_refused_before_the_browser_is_touched() {
        assert!(eval("browser-1", "   ").is_err());
        let err = click("browser-1", "  ").unwrap_err();
        assert!(err.contains("`selector` must not be empty"), "{err}");
        assert!(type_text("browser-1", "", "x").is_err());
        assert!(element_center("browser-1", "").is_err());
        assert!(close("browser-does-not-exist").is_err());
    }

    /// The one test that talks to a real browser. Ignored by default: `cargo test` must pass on
    /// a machine with no Chromium, and must not leave a process running.
    ///
    ///     cargo test -p vikram-runtime --lib -- --ignored --nocapture
    #[test]
    #[ignore = "launches a real headless browser"]
    fn a_session_can_be_driven_end_to_end() {
        let page =
            std::env::temp_dir().join(format!("vikram-cdp-smoke-{}.html", std::process::id()));
        std::fs::write(
                &page,
                r#"<!doctype html><html><body style="height:4000px">
                <h1 id="head">Hello</h1>
                <button id="go" onclick="if (event.isTrusted) document.getElementById('head').textContent='Clicked'">Go</button>
                <input id="q" type="text">
                <input id="hidden" type="text" style="display:none">
                <div id="out"></div>
                <script>
                document.getElementById('q').addEventListener('input', (e) => {
                  document.getElementById('out').textContent =
                    'value:' + e.target.value + ' trusted:' + e.isTrusted;
                });
                </script>
                </body></html>"#,
            )
            .unwrap();
        let url = format!("file://{}", page.display());
        let session = match open(&url) {
            Ok(session) => session,
            Err(e) => {
                eprintln!("no browser to drive: {e}");
                let _ = std::fs::remove_file(&page);
                return;
            }
        };
        assert!(list_sessions().contains(&session));
        assert!(text(&session).unwrap().contains("Hello"));
        assert_eq!(eval(&session, "1 + 1").unwrap(), "2");
        assert_eq!(eval(&session, "({a: 1})").unwrap(), r#"{"a":1}"#);
        // The button's handler only fires for a trusted event, which is the whole point of
        // going through `Input.dispatchMouseEvent` rather than `el.click()`.
        assert!(click(&session, "#go").unwrap().contains("clicked"));
        assert_eq!(
            eval(&session, "document.getElementById('head').textContent").unwrap(),
            "Clicked"
        );
        // The measurement is a viewport point, so it must be a real positive number.
        let (x, y) = element_center(&session, "#go").expect("a clickable button");
        assert!(x > 0.0 && y > 0.0, "{x},{y}");
        assert!(
            type_text(&session, "#q", "abc")
                .unwrap()
                .contains("3 characters")
        );
        // `isTrusted` on the page's own `input` event is what proves the typing was real.
        assert_eq!(
            eval(&session, "document.getElementById('out').textContent").unwrap(),
            "value:abc trusted:true"
        );
        assert!(scroll(&session, 0, 300).unwrap().starts_with("scrolled by"));
        assert!(key(&session, "Tab").unwrap().contains("pressed"));
        let err = click(&session, "#missing").unwrap_err();
        assert!(err.contains("selector matched no element"), "{err}");
        let err = click(&session, "#hidden").unwrap_err();
        assert!(err.contains("element has no size"), "{err}");
        let err = eval(&session, "null.x").unwrap_err();
        assert!(err.contains("TypeError"), "{err}");
        assert!(
            describe_sessions()
                .iter()
                .any(|line| line.contains(&session))
        );

        close(&session).unwrap();
        assert!(!list_sessions().contains(&session));
        assert!(
            close(&session).is_err(),
            "a closed session cannot be closed twice"
        );

        // A browser that dies under an open session is reported once, and the dead session is
        // dropped rather than left to fail on every later call.
        let doomed = open(&url).unwrap();
        if let Ok(mut reg) = registry().lock() {
            let browser = reg.get_mut(&doomed).expect("session in the registry");
            browser
                .child
                .kill()
                .expect("the browser process is killable");
        }
        std::thread::sleep(std::time::Duration::from_millis(200));
        let err = text(&doomed).unwrap_err();
        assert!(!err.is_empty(), "{err}");
        assert!(!list_sessions().contains(&doomed), "{:?}", list_sessions());
        let _ = std::fs::remove_file(&page);
    }

    #[test]
    fn opening_with_a_binary_that_does_not_exist_fails_without_leaking_a_session() {
        let before = count();
        let err = open_with(
            Path::new("/nonexistent/vikram-chromium"),
            "https://example.com",
        )
        .unwrap_err();
        assert!(err.contains("cannot start"), "{err}");
        assert_eq!(count(), before);
    }
}
