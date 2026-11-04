//! Desktop "computer use" on Linux, driven through the standard X11/Wayland helper programs.
//!
//! There is no window-server protocol in this crate: the tools shell out to the same small
//! command-line helpers a person would use — `xdotool` for input, `scrot`/`import`/`grim` for
//! screenshots and `wmctrl` for the window list. Everything degrades explicitly: a missing helper
//! produces an error naming the package to install, never a silent no-op, and no helper is
//! installed by us.
//!
//! Two properties are deliberate:
//!
//! * **No shell.** Every helper is spawned with an argv vector, so a title, a path or a chunk of
//!   text cannot turn into a second command. `computer.type --clearmodifiers -- {text}` is one
//!   argument, whatever the text contains.
//! * **Bounded.** Every helper invocation is killed after [`TIMEOUT_SECS`] seconds, because a
//!   window manager that is wedged must not wedge the turn that asked for a click.
//!
//! Text can be read back out of an image, but that is the whole of the "understanding" here:
//! [`ocr`] runs `tesseract` over a file and [`ocr_screen`] screenshots first, so the model can read
//! what is on screen instead of guessing from window titles. Nothing *locates* anything: there is no
//! element search, no widget tree and no coordinates suggestion, so the model still has to work out
//! where things are and say what to click.
//!
//! X11 support is complete; Wayland is partial by construction, because a Wayland compositor does
//! not let an ordinary client inject input. Screenshots fall back to `grim`, and input prefers
//! `ydotool` — which injects through the kernel's `uinput` and therefore reaches native Wayland
//! clients — when the session is Wayland and `ydotool` is installed. `xdotool` only reaches
//! XWayland clients, and it is still the only way to send a wheel click or an `xdotool`-style key
//! combination. [`status`] says all of this on a Wayland session instead of pretending.

use serde_json::{Value, json};
use std::path::{Path, PathBuf};
use std::process::{Command, Output, Stdio};
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};
use vikram_contracts::{ActionKind, PermissionRequest, SideEffect, Trust, VikramError};

use crate::tools::{Tool, ToolContext, ToolOutput, ToolRegistry, ToolSpec};

/// Wall-clock limit for a single helper invocation.
pub const TIMEOUT_SECS: u64 = 10;

/// OCR is not an input event: `tesseract` may load a language model and work over a full-screen
/// image, so it gets its own, longer budget instead of [`TIMEOUT_SECS`].
pub const OCR_TIMEOUT_SECS: u64 = 30;

/// Longer than this and `xdotool type` is measured in minutes rather than seconds.
const MAX_TYPED_CHARS: usize = 10_000;

/// The helper programs this module can drive. Probed in this order for the status report.
///
/// `tesseract` (OCR) and `ydotool` (Wayland-friendly input) are optional in the same way as the
/// rest: nothing here installs them, and a missing one is reported as an install hint.
pub const HELPERS: [&str; 7] = [
    "xdotool",
    "scrot",
    "import",
    "grim",
    "wmctrl",
    "tesseract",
    "ydotool",
];

/// The package that provides each helper.
fn install_hint(name: &str) -> &'static str {
    match name {
        "xdotool" => "apt install xdotool",
        "scrot" => "apt install scrot",
        "import" => "apt install imagemagick",
        "grim" => "apt install grim",
        "wmctrl" => "apt install wmctrl",
        "tesseract" => "apt install tesseract-ocr",
        "ydotool" => "apt install ydotool",
        _ => "install it and keep it on PATH",
    }
}

/// The message a caller sees when a helper is not installed. Names the fix, not just the symptom.
pub fn missing_tool_message(name: &str) -> String {
    format!("{name} not found; install it: {}", install_hint(name))
}

/// Resolve an executable by name on `PATH`. Only regular, executable files count, so a stale
/// directory entry cannot masquerade as an installed tool.
pub fn which(name: &str) -> Option<PathBuf> {
    if name.trim().is_empty() {
        return None;
    }
    let path = std::env::var_os("PATH")?;
    let candidates: Vec<String> = if cfg!(windows) && !name.contains('.') {
        let exts = std::env::var("PATHEXT").unwrap_or_else(|_| ".EXE;.CMD;.BAT;.COM".to_owned());
        std::iter::once(name.to_owned())
            .chain(
                exts.split(';')
                    .filter(|e| !e.is_empty())
                    .map(|e| format!("{name}{e}")),
            )
            .collect()
    } else {
        vec![name.to_owned()]
    };
    std::env::split_paths(&path)
        .filter(|dir| !dir.as_os_str().is_empty())
        .flat_map(|dir| candidates.iter().map(move |c| dir.join(c)))
        .find(|candidate| is_executable(candidate))
}

#[cfg(unix)]
fn is_executable(path: &Path) -> bool {
    use std::os::unix::fs::PermissionsExt;
    match std::fs::metadata(path) {
        Ok(meta) => meta.is_file() && meta.permissions().mode() & 0o111 != 0,
        Err(_) => false,
    }
}

#[cfg(not(unix))]
fn is_executable(path: &Path) -> bool {
    path.is_file()
}

/// Resolve a helper or explain how to install it.
pub fn require_tool(name: &str) -> Result<PathBuf, String> {
    which(name).ok_or_else(|| missing_tool_message(name))
}

/// The helpers that are actually installed, in [`HELPERS`] order.
pub fn available() -> Vec<&'static str> {
    HELPERS
        .into_iter()
        .filter(|helper| which(helper).is_some())
        .collect()
}

/// The first screenshot helper that is installed, if any.
///
/// The order follows the session. On Wayland only `grim` can see the whole compositor, so the X11
/// helpers are a fallback there (they capture XWayland clients at best); on X11 `scrot` is the
/// lightest option, then ImageMagick's `import`.
fn screenshot_helper() -> Option<&'static str> {
    let order: [&'static str; 3] = if session_kind() == "wayland" {
        ["grim", "scrot", "import"]
    } else {
        ["scrot", "import", "grim"]
    };
    order.into_iter().find(|helper| which(helper).is_some())
}

/// The first window-listing helper that is installed, if any.
fn window_helper() -> Option<&'static str> {
    ["wmctrl", "xdotool"]
        .into_iter()
        .find(|helper| which(helper).is_some())
}

/// Which display protocol this process can talk to, as far as the environment admits.
///
/// `XDG_SESSION_TYPE` is authoritative when a session manager set it; otherwise the presence of a
/// socket variable decides. Both being set means XWayland, which counts as Wayland here because
/// that is what the *compositor* is and therefore what screenshots and input must cope with.
fn session_kind() -> &'static str {
    match std::env::var("XDG_SESSION_TYPE")
        .as_deref()
        .map(str::trim)
        .unwrap_or_default()
    {
        "wayland" => return "wayland",
        "x11" => return "x11",
        _ => {}
    }
    if std::env::var_os("WAYLAND_DISPLAY").is_some() {
        "wayland"
    } else if std::env::var_os("DISPLAY").is_some() {
        "x11"
    } else {
        "headless (neither DISPLAY nor WAYLAND_DISPLAY is set)"
    }
}

/// Whether synthetic input should go through `ydotool` rather than `xdotool`.
///
/// Only on a Wayland session, and only when `ydotool` is installed: on X11 `xdotool` is strictly
/// more capable (`ydotool` has no wheel and no `ctrl+l`-style key syntax), so it stays the default.
fn prefer_ydotool() -> bool {
    session_kind() == "wayland" && which("ydotool").is_some()
}

/// Whether `combo` already reads as `ydotool` syntax (`KEY_LEFTCTRL+KEY_L`), which is the only form
/// `ydotool key` understands.
///
/// An `xdotool` combination such as `ctrl+l` is deliberately *not* translated: the mapping from
/// keysym names to evdev key names is large and lossy, and a wrong guess sends the wrong keys.
fn looks_like_ydotool_combo(combo: &str) -> bool {
    let combo = combo.trim();
    if combo.is_empty() || combo.starts_with('-') || combo.ends_with('+') {
        return false;
    }
    combo.split('+').all(|part| {
        let part = part.trim();
        match part.get(..4) {
            Some(prefix) if prefix.eq_ignore_ascii_case("KEY_") => part.len() > 4,
            _ => false,
        }
    })
}

/// Run one helper and collect its raw output, killing it after `timeout`.
fn run_raw(
    program: &str,
    args: &[&str],
    cwd: Option<&Path>,
    timeout: Duration,
) -> Result<Output, String> {
    let mut command = Command::new(program);
    command
        .args(args)
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped());
    if let Some(dir) = cwd {
        command.current_dir(dir);
    }

    let mut child = command.spawn().map_err(|e| {
        if e.kind() == std::io::ErrorKind::NotFound {
            missing_tool_message(program)
        } else {
            format!("cannot run `{program}`: {e}")
        }
    })?;

    let deadline = Instant::now() + timeout;
    loop {
        match child.try_wait() {
            Ok(Some(_)) => break,
            Ok(None) => {
                if Instant::now() >= deadline {
                    let _ = child.kill();
                    let _ = child.wait();
                    return Err(format!(
                        "`{program}` timed out after {}s",
                        timeout.as_secs()
                    ));
                }
                std::thread::sleep(Duration::from_millis(25));
            }
            Err(e) => return Err(format!("`{program}` failed while waiting: {e}")),
        }
    }

    child
        .wait_with_output()
        .map_err(|e| format!("`{program}` failed while waiting: {e}"))
}

/// Run one helper and collect its output, killing it after [`TIMEOUT_SECS`].
///
/// Returns `Err` for a missing binary, a timeout, or a non-zero exit — the three ways a helper
/// fails that the caller can act on.
fn run_command(program: &str, args: &[&str], cwd: Option<&Path>) -> Result<String, String> {
    let output = run_raw(program, args, cwd, Duration::from_secs(TIMEOUT_SECS))?;
    let mut body = String::from_utf8_lossy(&output.stdout).into_owned();
    let stderr = String::from_utf8_lossy(&output.stderr);
    if !stderr.trim().is_empty() {
        if !body.is_empty() && !body.ends_with('\n') {
            body.push('\n');
        }
        body.push_str("[stderr]\n");
        body.push_str(&stderr);
    }
    if !output.status.success() {
        let detail = if body.trim().is_empty() {
            format!("exit {:?}", output.status.code())
        } else {
            body.trim().to_string()
        };
        return Err(format!("`{program}` failed: {detail}"));
    }
    Ok(body)
}

/// Run one helper and return only its standard output, on success.
///
/// `tesseract` writes progress chatter to stderr even when it succeeds, so OCR must not take the
/// mixed report [`run_command`] builds — the recognised text would arrive next to log noise.
fn run_stdout_only(program: &str, args: &[&str], timeout: Duration) -> Result<String, String> {
    let output = run_raw(program, args, None, timeout)?;
    let stdout = String::from_utf8_lossy(&output.stdout).into_owned();
    if !output.status.success() {
        let stderr = String::from_utf8_lossy(&output.stderr);
        let detail = if !stderr.trim().is_empty() {
            stderr.trim().to_string()
        } else if !stdout.trim().is_empty() {
            stdout.trim().to_string()
        } else {
            format!("exit {:?}", output.status.code())
        };
        return Err(format!("`{program}` failed: {detail}"));
    }
    Ok(stdout)
}

fn no_screenshot_helper() -> String {
    "no screenshot helper found; install one: apt install scrot (X11), apt install grim (Wayland) \
     or apt install imagemagick"
        .to_string()
}

fn no_window_helper() -> String {
    "no window listing helper found; install one: apt install wmctrl or apt install xdotool"
        .to_string()
}

/// Capture the whole screen to `path`.
///
/// `scrot` (X11) is preferred, then ImageMagick's `import`, then `grim` (Wayland).
pub fn screenshot(path: &str) -> Result<String, String> {
    let path = path.trim();
    if path.is_empty() {
        return Err("screenshot needs a destination path".to_string());
    }
    let helper = screenshot_helper().ok_or_else(no_screenshot_helper)?;
    let args: Vec<&str> = match helper {
        // `import` needs the target spelled out; the other two default to the whole screen.
        "import" => vec!["-window", "root", path],
        _ => vec![path],
    };
    run_command(helper, &args, None)?;
    Ok(format!("screenshot saved to {path} via {helper}"))
}

/// A fresh path for a screen capture, in the system temp directory.
///
/// The process id and a nanosecond stamp keep two concurrent calls from overwriting each other's
/// image; the file is created by the screenshot helper, so no handle is opened here.
fn temp_screenshot_path() -> PathBuf {
    let stamp = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|elapsed| elapsed.as_nanos())
        .unwrap_or(0);
    std::env::temp_dir().join(format!("vikram-screen-{}-{stamp}.png", std::process::id()))
}

/// Recognise the text in an image file with `tesseract`.
///
/// Only stdout is returned: `tesseract` prints progress chatter to stderr even on success, and that
/// is not part of the recognised text. An image with no text is a success with an explanatory
/// message, not an error.
pub fn ocr(path: &str) -> Result<String, String> {
    let path = path.trim();
    if path.is_empty() {
        return Err("ocr needs a path to an image file".to_string());
    }
    let text = ocr_text(path)?;
    let body = text.trim();
    if body.is_empty() {
        return Ok(format!("no text recognised in {path}"));
    }
    Ok(truncated(body.to_string()))
}

/// The raw recognised text, which may legitimately be empty.
fn ocr_text(path: &str) -> Result<String, String> {
    require_tool("tesseract")?;
    if !Path::new(path).is_file() {
        return Err(format!("no such image file: {path}"));
    }
    run_stdout_only(
        "tesseract",
        &[path, "stdout"],
        Duration::from_secs(OCR_TIMEOUT_SECS),
    )
}

/// Screenshot the whole screen, OCR it, and remove the temporary image.
///
/// `tesseract` is checked before the capture so a machine without it writes no file and gets the
/// install hint rather than a dangling screenshot path.
pub fn ocr_screen() -> Result<String, String> {
    require_tool("tesseract")?;
    let path = temp_screenshot_path();
    let target = path.to_string_lossy().into_owned();
    if let Err(err) = screenshot(&target) {
        let _ = std::fs::remove_file(&path);
        return Err(err);
    }
    let text = ocr_text(&target);
    let _ = std::fs::remove_file(&path);
    let body = text?.trim().to_string();
    if body.is_empty() {
        return Ok("no text recognised on screen".to_string());
    }
    Ok(truncated(body))
}

/// Move the pointer to `(x, y)` and click `button` (1 left, 2 middle, 3 right).
///
/// On a Wayland session with `ydotool` installed, the move and the click go through `ydotool`
/// (an absolute move followed by a click, since `ydotool` has no combined form); otherwise through
/// `xdotool`.
pub fn click(x: i64, y: i64, button: u8) -> Result<String, String> {
    if x < 0 || y < 0 {
        return Err(format!(
            "screen coordinates must be non-negative, got ({x}, {y})"
        ));
    }
    if !(1..=5).contains(&button) {
        return Err(format!(
            "button must be 1 (left), 2 (middle), 3 (right) or 4/5 (wheel), got {button}"
        ));
    }
    let (x_text, y_text, button_text) = (x.to_string(), y.to_string(), button.to_string());
    if prefer_ydotool() {
        run_command(
            "ydotool",
            &["mousemove", "--absolute", &x_text, &y_text],
            None,
        )?;
        run_command("ydotool", &["click", &button_text], None)?;
        return Ok(format!(
            "clicked button {button_text} at ({x_text}, {y_text}) via ydotool"
        ));
    }
    require_tool("xdotool")?;
    run_command(
        "xdotool",
        &["mousemove", &x_text, &y_text, "click", &button_text],
        None,
    )?;
    Ok(format!(
        "clicked button {button_text} at ({x_text}, {y_text})"
    ))
}

/// Type `text` into whatever window has focus.
///
/// The text is passed as a single argv element after `--`, so leading dashes, quotes and
/// semicolons arrive at the key injection layer literally. On a Wayland session with `ydotool`
/// installed the text goes through `ydotool type`, which can reach native Wayland clients.
pub fn type_text(text: &str) -> Result<String, String> {
    if text.is_empty() {
        return Err("type needs non-empty text".to_string());
    }
    let length = text.chars().count();
    if length > MAX_TYPED_CHARS {
        return Err(format!(
            "text is {length} characters; the limit for a single call is {MAX_TYPED_CHARS}"
        ));
    }
    if prefer_ydotool() {
        run_command("ydotool", &["type", "--", text], None)?;
        return Ok(format!("typed {length} characters via ydotool"));
    }
    require_tool("xdotool")?;
    run_command("xdotool", &["type", "--clearmodifiers", "--", text], None)?;
    Ok(format!("typed {length} characters"))
}

/// Send a key combination such as `ctrl+l` or `super+Return`.
///
/// `ydotool` spells key names differently (`KEY_LEFTCTRL+KEY_L`), and translating an `xdotool`
/// combination into evdev names is a large, lossy mapping that would sometimes send the wrong keys.
/// So a combination already in `ydotool` form is passed to `ydotool` on a Wayland session, and
/// everything else stays with `xdotool` — which reaches XWayland windows only.
pub fn key(combo: &str) -> Result<String, String> {
    let combo = combo.trim();
    if combo.is_empty() {
        return Err("key needs a combination, e.g. `ctrl+l` or `super+Return`".to_string());
    }
    if combo.starts_with('-') {
        return Err(format!(
            "`{combo}` is not a key combination (it would be read as an option)"
        ));
    }
    if prefer_ydotool() && looks_like_ydotool_combo(combo) {
        run_command("ydotool", &["key", combo], None)?;
        return Ok(format!("sent key {combo} via ydotool"));
    }
    if session_kind() == "wayland" && which("ydotool").is_some() && which("xdotool").is_none() {
        return Err(format!(
            "`{combo}` is xdotool syntax and xdotool is not installed; ydotool needs evdev key \
             names instead, e.g. `KEY_LEFTCTRL+KEY_L` ({})",
            install_hint("xdotool")
        ));
    }
    require_tool("xdotool")?;
    run_command("xdotool", &["key", "--clearmodifiers", combo], None)?;
    Ok(if session_kind() == "wayland" {
        format!(
            "sent key {combo} via xdotool (Wayland: this reaches XWayland windows only; spell the \
             combination as `KEY_LEFTCTRL+KEY_L` to route it through ydotool)"
        )
    } else {
        format!("sent key {combo}")
    })
}

/// Scroll `amount` steps in `direction` (`up` or `down`). One step is one wheel click.
///
/// A wheel click is an X11 input event with no `ydotool` equivalent, so scrolling stays on
/// `xdotool` even when the rest of the input path prefers `ydotool` on Wayland — where it therefore
/// reaches XWayland clients only.
pub fn scroll(direction: &str, amount: u32) -> Result<String, String> {
    let direction = direction.trim().to_ascii_lowercase();
    let button = match direction.as_str() {
        "up" => "4",
        "down" => "5",
        other => {
            return Err(format!("direction must be `up` or `down`, got `{other}`"));
        }
    };
    if amount == 0 {
        return Err("amount must be at least 1".to_string());
    }
    if amount > 50 {
        return Err(format!("amount must be 50 or less, got {amount}"));
    }
    if which("xdotool").is_none() {
        return Err(if prefer_ydotool() {
            "scrolling needs xdotool: ydotool has no wheel event; install it: apt install xdotool"
                .to_string()
        } else {
            missing_tool_message("xdotool")
        });
    }
    for _ in 0..amount {
        run_command("xdotool", &["click", button], None)?;
    }
    Ok(if session_kind() == "wayland" {
        format!(
            "scrolled {direction} {amount} step(s) via xdotool (Wayland: wheel clicks reach \
             XWayland windows only)"
        )
    } else {
        format!("scrolled {direction} {amount} step(s)")
    })
}

/// List the visible windows by title.
pub fn windows() -> Result<String, String> {
    let helper = window_helper().ok_or_else(no_window_helper)?;
    let output = match helper {
        "wmctrl" => run_command("wmctrl", &["-l"], None)?,
        _ => run_command(
            "xdotool",
            &["search", "--name", "", "getwindowname", "%@"],
            None,
        )?,
    };
    let body = output.trim();
    if body.is_empty() {
        return Ok(format!("no windows reported (session: {})", session_kind()));
    }
    Ok(truncated(body.to_string()))
}

/// Which program synthetic input would actually go through, and why.
fn input_summary() -> String {
    let has_xdotool = which("xdotool").is_some();
    let has_ydotool = which("ydotool").is_some();
    if prefer_ydotool() {
        return if has_xdotool {
            "ydotool (Wayland-friendly); xdotool for wheel clicks and xdotool-style key combinations"
                .to_string()
        } else {
            "ydotool (Wayland-friendly); no xdotool, so wheel clicks and xdotool-style key \
             combinations are unavailable"
                .to_string()
        };
    }
    if has_xdotool {
        return "xdotool".to_string();
    }
    if has_ydotool {
        return "ydotool (detected, but only preferred on a Wayland session)".to_string();
    }
    "unavailable (no xdotool; on Wayland install ydotool)".to_string()
}

/// A short report of which helpers exist and what they would be used for.
pub fn status() -> String {
    #[cfg(not(unix))]
    {
        return "desktop automation is not supported on this platform (Linux/X11/Wayland required).".to_owned();
    }
    let detected = available();
    let missing: Vec<String> = HELPERS
        .iter()
        .filter(|helper| !detected.contains(helper))
        .map(|helper| format!("{helper} ({})", install_hint(helper)))
        .collect();

    let mut out = format!(
        "detected: {}",
        if detected.is_empty() {
            "none".to_string()
        } else {
            detected.join(", ")
        }
    );
    if !missing.is_empty() {
        out.push_str(&format!("\nmissing: {}", missing.join(", ")));
    }
    out.push_str(&format!(
        "\nscreenshot: {}",
        screenshot_helper().unwrap_or("unavailable (no scrot, import or grim)")
    ));
    out.push_str(&format!("\ninput: {}", input_summary()));
    out.push_str(&format!(
        "\nocr: {}",
        if which("tesseract").is_some() {
            "tesseract (computer.ocr, computer.read_screen)"
        } else {
            "unavailable (no tesseract)"
        }
    ));
    out.push_str(&format!(
        "\nwindows: {}",
        window_helper().unwrap_or("unavailable (no wmctrl or xdotool)")
    ));
    out.push_str(&format!("\nsession: {}", session_kind()));
    if session_kind() == "wayland" {
        if which("ydotool").is_some() {
            out.push_str(
                "\nnote: input goes through ydotool, which reaches native Wayland clients; wheel \
                 clicks and xdotool-style key combinations still use xdotool and reach XWayland \
                 windows only",
            );
        } else {
            out.push_str(
                "\nnote: xdotool reaches XWayland windows only; a native Wayland client may ignore \
                 synthetic input (install ydotool for Wayland-native input)",
            );
        }
    }
    out
}

/// Tool output is unbounded from a busy desktop; cut it rather than flood the conversation.
fn truncated(mut text: String) -> String {
    const LIMIT: usize = 20_000;
    if text.len() > LIMIT {
        let cut = text
            .char_indices()
            .take_while(|(i, _)| *i < LIMIT)
            .last()
            .map(|(i, c)| i + c.len_utf8())
            .unwrap_or(0);
        text.truncate(cut);
        text.push_str("\n… [output truncated]");
    }
    text
}

fn arg_str(args: &Value, key: &str) -> Result<String, VikramError> {
    args.get(key)
        .and_then(Value::as_str)
        .map(str::to_owned)
        .ok_or_else(|| VikramError::ProviderRejected(format!("missing string argument `{key}`")))
}

fn arg_int(args: &Value, key: &str) -> Result<i64, VikramError> {
    args.get(key)
        .and_then(Value::as_i64)
        .ok_or_else(|| VikramError::ProviderRejected(format!("missing integer argument `{key}`")))
}

/// Turn a module-level `Result` into tool output: a refusal to act is `ok: false`, not an error
/// of the runtime itself.
fn reported(result: Result<String, String>) -> ToolOutput {
    match result {
        Ok(content) => ToolOutput {
            content: truncated(content),
            ok: true,
        },
        Err(err) => ToolOutput {
            content: err,
            ok: false,
        },
    }
}

struct Screenshot;

impl Tool for Screenshot {
    fn spec(&self) -> ToolSpec {
        ToolSpec {
            name: "computer.screenshot".into(),
            description: "Capture the whole screen to an image file (scrot, ImageMagick import or \
                          grim). Returns the path written."
                .into(),
            schema: json!({
                "type": "object",
                "properties": {
                    "path": {
                        "type": "string",
                        "description": "Destination file; a relative path is resolved against the workspace root."
                    }
                },
                "required": ["path"]
            }),
        }
    }

    fn permission(&self, args: &Value) -> Result<PermissionRequest, VikramError> {
        let path = arg_str(args, "path")?;
        Ok(PermissionRequest::new(
            ActionKind::Execute,
            "computer.screenshot",
            path,
            SideEffect::LocalMutation,
            Trust::Model,
        ))
    }

    fn run(&self, args: &Value, ctx: &ToolContext) -> Result<ToolOutput, VikramError> {
        let raw = arg_str(args, "path")?;
        let resolved = if Path::new(&raw).is_absolute() {
            PathBuf::from(&raw)
        } else {
            ctx.workspace_root.join(&raw)
        };
        Ok(reported(screenshot(&resolved.to_string_lossy())))
    }
}

struct Click;

impl Tool for Click {
    fn spec(&self) -> ToolSpec {
        ToolSpec {
            name: "computer.click".into(),
            description: "Move the pointer to (x, y) and click a mouse button (1 left, default)."
                .into(),
            schema: json!({
                "type": "object",
                "properties": {
                    "x": {"type": "integer", "description": "Screen X coordinate in pixels."},
                    "y": {"type": "integer", "description": "Screen Y coordinate in pixels."},
                    "button": {
                        "type": "integer",
                        "description": "1 left (default), 2 middle, 3 right."
                    }
                },
                "required": ["x", "y"]
            }),
        }
    }

    fn permission(&self, args: &Value) -> Result<PermissionRequest, VikramError> {
        let x = arg_int(args, "x")?;
        let y = arg_int(args, "y")?;
        Ok(PermissionRequest::new(
            ActionKind::Execute,
            "computer.click",
            format!("{x},{y}"),
            SideEffect::LocalMutation,
            Trust::Model,
        ))
    }

    fn run(&self, args: &Value, _ctx: &ToolContext) -> Result<ToolOutput, VikramError> {
        let x = arg_int(args, "x")?;
        let y = arg_int(args, "y")?;
        let button = args
            .get("button")
            .and_then(Value::as_i64)
            .unwrap_or(1)
            .clamp(0, u8::MAX as i64) as u8;
        Ok(reported(click(x, y, button)))
    }
}

struct TypeText;

impl Tool for TypeText {
    fn spec(&self) -> ToolSpec {
        ToolSpec {
            name: "computer.type".into(),
            description: "Type text into the focused window.".into(),
            schema: json!({
                "type": "object",
                "properties": {
                    "text": {"type": "string", "description": "Literal text to type."}
                },
                "required": ["text"]
            }),
        }
    }

    fn permission(&self, args: &Value) -> Result<PermissionRequest, VikramError> {
        let _ = arg_str(args, "text")?;
        Ok(PermissionRequest::new(
            ActionKind::Execute,
            "computer.type",
            "keyboard",
            SideEffect::LocalMutation,
            Trust::Model,
        ))
    }

    fn run(&self, args: &Value, _ctx: &ToolContext) -> Result<ToolOutput, VikramError> {
        let text = arg_str(args, "text")?;
        Ok(reported(type_text(&text)))
    }
}

struct Key;

impl Tool for Key {
    fn spec(&self) -> ToolSpec {
        ToolSpec {
            name: "computer.key".into(),
            description: "Send a key combination, e.g. `ctrl+l`, `alt+Tab`, `super+Return`.".into(),
            schema: json!({
                "type": "object",
                "properties": {
                    "keys": {"type": "string", "description": "xdotool key sequence."}
                },
                "required": ["keys"]
            }),
        }
    }

    fn permission(&self, args: &Value) -> Result<PermissionRequest, VikramError> {
        let keys = arg_str(args, "keys")?;
        Ok(PermissionRequest::new(
            ActionKind::Execute,
            "computer.key",
            keys,
            SideEffect::LocalMutation,
            Trust::Model,
        ))
    }

    fn run(&self, args: &Value, _ctx: &ToolContext) -> Result<ToolOutput, VikramError> {
        let keys = arg_str(args, "keys")?;
        Ok(reported(key(&keys)))
    }
}

struct Scroll;

impl Tool for Scroll {
    fn spec(&self) -> ToolSpec {
        ToolSpec {
            name: "computer.scroll".into(),
            description: "Scroll the focused window up or down by a number of wheel clicks.".into(),
            schema: json!({
                "type": "object",
                "properties": {
                    "direction": {"type": "string", "enum": ["up", "down"]},
                    "amount": {"type": "integer", "description": "Wheel clicks, default 3."}
                },
                "required": ["direction"]
            }),
        }
    }

    fn permission(&self, args: &Value) -> Result<PermissionRequest, VikramError> {
        let direction = arg_str(args, "direction")?;
        Ok(PermissionRequest::new(
            ActionKind::Execute,
            "computer.scroll",
            direction,
            SideEffect::LocalMutation,
            Trust::Model,
        ))
    }

    fn run(&self, args: &Value, _ctx: &ToolContext) -> Result<ToolOutput, VikramError> {
        let direction = arg_str(args, "direction")?;
        let amount = args
            .get("amount")
            .and_then(Value::as_u64)
            .unwrap_or(3)
            .min(u64::from(u32::MAX)) as u32;
        Ok(reported(scroll(&direction, amount)))
    }
}

struct Windows;

impl Tool for Windows {
    fn spec(&self) -> ToolSpec {
        ToolSpec {
            name: "computer.windows".into(),
            description: "List visible windows by title (wmctrl -l, or xdotool).".into(),
            schema: json!({"type": "object", "properties": {}}),
        }
    }

    fn permission(&self, _args: &Value) -> Result<PermissionRequest, VikramError> {
        Ok(PermissionRequest::new(
            ActionKind::Read,
            "computer.windows",
            "display",
            SideEffect::None,
            Trust::Model,
        ))
    }

    fn run(&self, _args: &Value, _ctx: &ToolContext) -> Result<ToolOutput, VikramError> {
        Ok(reported(windows()))
    }
}

struct Status;

impl Tool for Status {
    fn spec(&self) -> ToolSpec {
        ToolSpec {
            name: "computer.status".into(),
            description: "Report which desktop helper tools are installed and what they enable."
                .into(),
            schema: json!({"type": "object", "properties": {}}),
        }
    }

    fn permission(&self, _args: &Value) -> Result<PermissionRequest, VikramError> {
        Ok(PermissionRequest::new(
            ActionKind::Read,
            "computer.status",
            "display",
            SideEffect::None,
            Trust::Model,
        ))
    }

    fn run(&self, _args: &Value, _ctx: &ToolContext) -> Result<ToolOutput, VikramError> {
        Ok(ToolOutput {
            content: status(),
            ok: true,
        })
    }
}

struct Ocr;

impl Tool for Ocr {
    fn spec(&self) -> ToolSpec {
        ToolSpec {
            name: "computer.ocr".into(),
            description: "Extract the text from an image file with tesseract. Returns the \
                          recognised text."
                .into(),
            schema: json!({
                "type": "object",
                "properties": {
                    "path": {
                        "type": "string",
                        "description": "Image file to read; a relative path is resolved against the workspace root."
                    }
                },
                "required": ["path"]
            }),
        }
    }

    fn permission(&self, args: &Value) -> Result<PermissionRequest, VikramError> {
        let path = arg_str(args, "path")?;
        Ok(PermissionRequest::new(
            ActionKind::Read,
            "computer.ocr",
            path,
            SideEffect::None,
            Trust::Model,
        ))
    }

    fn run(&self, args: &Value, ctx: &ToolContext) -> Result<ToolOutput, VikramError> {
        let raw = arg_str(args, "path")?;
        let resolved = if Path::new(&raw).is_absolute() {
            PathBuf::from(&raw)
        } else {
            ctx.workspace_root.join(&raw)
        };
        Ok(reported(ocr(&resolved.to_string_lossy())))
    }
}

struct ReadScreen;

impl Tool for ReadScreen {
    fn spec(&self) -> ToolSpec {
        ToolSpec {
            name: "computer.read_screen".into(),
            description: "Screenshot the whole screen, run it through tesseract and return the \
                          recognised text. The temporary image is deleted afterwards."
                .into(),
            schema: json!({"type": "object", "properties": {}}),
        }
    }

    fn permission(&self, _args: &Value) -> Result<PermissionRequest, VikramError> {
        Ok(PermissionRequest::new(
            ActionKind::Read,
            "computer.read_screen",
            "display",
            SideEffect::None,
            Trust::Model,
        ))
    }

    fn run(&self, _args: &Value, _ctx: &ToolContext) -> Result<ToolOutput, VikramError> {
        Ok(reported(ocr_screen()))
    }
}

/// Register the desktop tools. Registered unconditionally: a machine without `xdotool` still gets
/// `computer.status`, and the input tools fail with an install hint rather than disappearing from
/// the tool list — a model that cannot see a tool cannot report that it is missing. The same holds
/// for `computer.ocr` and `computer.read_screen` without `tesseract`.
pub fn register(registry: &ToolRegistry) {
    registry.register(Box::new(Screenshot));
    registry.register(Box::new(Click));
    registry.register(Box::new(TypeText));
    registry.register(Box::new(Key));
    registry.register(Box::new(Scroll));
    registry.register(Box::new(Windows));
    registry.register(Box::new(Status));
    registry.register(Box::new(Ocr));
    registry.register(Box::new(ReadScreen));
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A name no package manager, PATH entry or test fixture will ever provide.
    const ABSENT: &str = "vikram-definitely-not-installed-9f3c";

    const ALL: [&str; 9] = [
        "computer.screenshot",
        "computer.click",
        "computer.type",
        "computer.key",
        "computer.scroll",
        "computer.windows",
        "computer.status",
        "computer.ocr",
        "computer.read_screen",
    ];

    fn ctx() -> ToolContext {
        ToolContext {
            workspace_root: PathBuf::from("."),
            sandbox: crate::sandbox::Sandbox::disabled("test context"),
            spill: crate::spill::Spill::disabled(),
            runtime: None,
            session: None,
        }
    }

    // --- helper detection ----------------------------------------------------------------------
    //
    // Detection is exercised against whatever this machine happens to have installed: the tests
    // assert the shape of the answer and the behaviour for a tool that is certainly absent, never
    // that a particular helper exists.

    #[test]
    fn helper_detection_does_not_panic() {
        for helper in HELPERS {
            let _ = which(helper);
        }
        let _ = which("");
        let _ = which("   ");
        let _ = which(ABSENT);
        let _ = screenshot_helper();
        let _ = window_helper();
        let _ = session_kind();
        let _ = prefer_ydotool();
        let _ = input_summary();
        let _ = temp_screenshot_path();
        let _ = looks_like_ydotool_combo("");
        let _ = looks_like_ydotool_combo("KEY_LEFTCTRL+KEY_L");
        let _ = looks_like_ydotool_combo("ctrl+l");
    }

    #[test]
    fn available_returns_only_known_helpers() {
        let detected: Vec<&'static str> = available();
        assert!(
            detected.iter().all(|helper| HELPERS.contains(helper)),
            "{detected:?}"
        );
        // Order is the probe order, so the list is stable for a given machine.
        let mut sorted = detected.clone();
        sorted.sort_unstable_by_key(|helper| HELPERS.iter().position(|h| h == helper));
        assert_eq!(detected, sorted);
    }

    #[test]
    fn available_reports_the_ocr_and_wayland_helpers_when_they_are_installed() {
        // Whatever this machine has, detection of tesseract and ydotool must agree with `which`.
        let detected = available();
        for helper in ["tesseract", "ydotool"] {
            assert_eq!(
                detected.contains(&helper),
                which(helper).is_some(),
                "`{helper}` availability disagrees with `which`"
            );
        }
    }

    #[test]
    fn ydotool_is_only_preferred_on_wayland_with_ydotool_installed() {
        let expected = session_kind() == "wayland" && which("ydotool").is_some();
        assert_eq!(prefer_ydotool(), expected);
        // X11 must keep xdotool as the injector even where ydotool exists.
        if session_kind() == "x11" {
            assert!(!prefer_ydotool());
        }
    }

    #[test]
    fn only_evdev_style_combinations_are_treated_as_ydotool_syntax() {
        assert!(looks_like_ydotool_combo("KEY_LEFTCTRL+KEY_L"));
        assert!(looks_like_ydotool_combo("KEY_LEFTMETA+KEY_RETURN"));
        assert!(looks_like_ydotool_combo("KEY_A"));
        assert!(looks_like_ydotool_combo("key_leftctrl+key_l"));
        for xdotool_style in ["ctrl+l", "super+Return", "", "   ", "-x", "KEY_+", "ctrl"] {
            assert!(
                !looks_like_ydotool_combo(xdotool_style),
                "`{xdotool_style}` must not be passed to ydotool"
            );
        }
    }

    #[test]
    fn chosen_helpers_come_from_the_sets_the_docs_promise() {
        if let Some(helper) = screenshot_helper() {
            assert!(
                ["scrot", "import", "grim"].contains(&helper),
                "unexpected screenshot helper `{helper}`"
            );
        }
        if let Some(helper) = window_helper() {
            assert!(
                ["wmctrl", "xdotool"].contains(&helper),
                "unexpected window helper `{helper}`"
            );
        }
        let session = session_kind();
        assert!(
            session == "x11" || session == "wayland" || session.starts_with("headless"),
            "unexpected session kind `{session}`"
        );
    }

    #[test]
    fn a_missing_tool_names_the_package_to_install() {
        let err = require_tool(ABSENT).unwrap_err();
        assert!(err.contains(ABSENT), "{err}");
        assert!(err.contains("not found"), "{err}");
        assert!(err.contains("install it"), "{err}");
    }

    #[test]
    fn every_helper_has_a_concrete_install_hint() {
        assert_eq!(
            missing_tool_message("xdotool"),
            "xdotool not found; install it: apt install xdotool"
        );
        assert!(missing_tool_message("scrot").contains("apt install scrot"));
        assert!(missing_tool_message("import").contains("apt install imagemagick"));
        assert!(missing_tool_message("grim").contains("apt install grim"));
        assert!(missing_tool_message("wmctrl").contains("apt install wmctrl"));
        assert_eq!(
            missing_tool_message("tesseract"),
            "tesseract not found; install it: apt install tesseract-ocr"
        );
        assert!(missing_tool_message("ydotool").contains("apt install ydotool"));
        assert!(missing_tool_message("mystery").contains("PATH"));
    }

    #[test]
    fn the_ocr_and_wayland_helpers_explain_themselves_when_absent() {
        // The install hints the new helpers surface. Built from the same table, so these hold
        // whether or not the helper happens to be installed on this machine.
        assert_eq!(
            missing_tool_message("tesseract"),
            "tesseract not found; install it: apt install tesseract-ocr"
        );
        assert_eq!(
            missing_tool_message("ydotool"),
            "ydotool not found; install it: apt install ydotool"
        );
        assert_eq!(
            require_tool(ABSENT).unwrap_err(),
            missing_tool_message(ABSENT)
        );
    }

    #[test]
    fn ocr_and_read_screen_report_the_install_hint_when_tesseract_is_absent() {
        if which("tesseract").is_some() {
            // This machine would really run OCR, and tests must not depend on a desktop or on the
            // language-model files. The hint text itself is covered above.
            return;
        }
        let err = ocr("/no/such/image.png").unwrap_err();
        assert!(err.contains("tesseract not found"), "{err}");
        assert!(err.contains("apt install tesseract-ocr"), "{err}");

        // `read_screen` checks tesseract before capturing, so this never touches a display.
        let out = ReadScreen.run(&json!({}), &ctx()).unwrap();
        assert!(!out.ok, "{}", out.content);
        assert!(
            out.content.contains("tesseract not found"),
            "{}",
            out.content
        );
        assert!(
            out.content.contains("apt install tesseract-ocr"),
            "{}",
            out.content
        );
    }

    #[test]
    fn running_an_absent_command_fails_with_the_install_hint() {
        let err = run_command(ABSENT, &["--version"], None).unwrap_err();
        assert!(err.contains("not found"), "{err}");
        assert!(err.contains("install it"), "{err}");
    }

    #[test]
    fn a_failing_command_reports_its_exit_and_stderr() {
        // `false` and `sh` are on PATH on every host this crate supports, and neither needs a
        // display, so this path is testable without a desktop session.
        let err = run_command("sh", &["-c", "echo boom >&2; exit 3"], None).unwrap_err();
        assert!(err.contains("failed"), "{err}");
        assert!(err.contains("boom"), "{err}");
    }

    // --- argument validation before any effect -------------------------------------------------

    #[test]
    fn screenshot_requires_a_destination() {
        assert!(screenshot("").is_err());
        assert!(screenshot("   ").is_err());
    }

    #[test]
    fn ocr_requires_a_path() {
        // Refused before `tesseract` is even looked up, so no OCR runs and no desktop is needed.
        assert!(ocr("").is_err());
        assert!(ocr("   ").is_err());
        let err = ocr("").unwrap_err();
        assert!(err.contains("image file"), "{err}");
    }

    #[test]
    fn click_rejects_impossible_coordinates_and_buttons() {
        assert!(click(-1, 0, 1).is_err());
        assert!(click(0, -1, 1).is_err());
        assert!(click(0, 0, 0).is_err());
        assert!(click(0, 0, 6).is_err());
    }

    #[test]
    fn typing_rejects_empty_and_enormous_text() {
        assert!(type_text("").is_err());
        let huge = "a".repeat(MAX_TYPED_CHARS + 1);
        let err = type_text(&huge).unwrap_err();
        assert!(err.contains(&MAX_TYPED_CHARS.to_string()), "{err}");
    }

    #[test]
    fn key_rejects_empty_and_option_like_combinations() {
        assert!(key("").is_err());
        assert!(key("   ").is_err());
        assert!(key("--help").is_err());
    }

    #[test]
    fn scroll_rejects_unknown_directions_and_wild_amounts() {
        assert!(scroll("sideways", 1).is_err());
        assert!(scroll("up", 0).is_err());
        assert!(scroll("down", 51).is_err());
    }

    // --- registration on the real registry ------------------------------------------------------

    #[test]
    fn every_computer_tool_is_registered_with_builtins() {
        let registry = ToolRegistry::with_builtins();
        let names = registry.names();
        for name in ALL {
            assert!(
                names.contains(&name.to_string()),
                "`{name}` must be registered; registry has {names:?}"
            );
        }
    }

    #[test]
    fn input_tools_declare_execute_with_a_local_mutation() {
        let registry = ToolRegistry::with_builtins();
        let cases = [
            ("computer.screenshot", json!({"path": "shot.png"})),
            ("computer.click", json!({"x": 10, "y": 20})),
            ("computer.type", json!({"text": "hello"})),
            ("computer.key", json!({"keys": "ctrl+l"})),
            ("computer.scroll", json!({"direction": "down"})),
        ];
        for (name, args) in cases {
            let req = registry.get(name).expect(name).permission(&args).unwrap();
            assert_eq!(req.action, ActionKind::Execute, "{name}");
            assert_eq!(req.verb, name, "{name}");
            assert_eq!(req.side_effect, SideEffect::LocalMutation, "{name}");
            assert_eq!(req.originating_trust, Trust::Model, "{name}");
        }
    }

    #[test]
    fn inspection_tools_declare_read_without_a_side_effect() {
        let registry = ToolRegistry::with_builtins();
        for name in ["computer.windows", "computer.status"] {
            let req = registry
                .get(name)
                .expect(name)
                .permission(&json!({}))
                .unwrap();
            assert_eq!(req.action, ActionKind::Read, "{name}");
            assert_eq!(req.verb, name, "{name}");
            assert_eq!(req.side_effect, SideEffect::None, "{name}");
        }
    }

    #[test]
    fn ocr_and_read_screen_are_registered_and_read_only() {
        let registry = ToolRegistry::with_builtins();
        let names = registry.names();
        for name in ["computer.ocr", "computer.read_screen"] {
            assert!(
                names.contains(&name.to_string()),
                "{name} is not registered"
            );
        }

        let req = registry
            .get("computer.ocr")
            .unwrap()
            .permission(&json!({"path": "shots/a.png"}))
            .unwrap();
        assert_eq!(req.action, ActionKind::Read);
        assert_eq!(req.verb, "computer.ocr");
        assert_eq!(req.resource, "shots/a.png");
        assert_eq!(req.side_effect, SideEffect::None);
        assert_eq!(req.originating_trust, Trust::Model);

        // `read_screen` takes no arguments at all, so its shape must not depend on any.
        let req = registry
            .get("computer.read_screen")
            .unwrap()
            .permission(&json!({}))
            .unwrap();
        assert_eq!(req.action, ActionKind::Read);
        assert_eq!(req.verb, "computer.read_screen");
        assert_eq!(req.resource, "display");
        assert_eq!(req.side_effect, SideEffect::None);
        assert_eq!(req.originating_trust, Trust::Model);
    }

    #[test]
    fn the_ocr_schemas_document_what_the_tools_need() {
        let registry = ToolRegistry::with_builtins();
        let ocr_spec = registry.get("computer.ocr").unwrap().spec();
        assert_eq!(ocr_spec.name, "computer.ocr");
        assert_eq!(ocr_spec.schema["type"], "object");
        assert!(ocr_spec.schema["properties"]["path"].is_object());
        let required = ocr_spec.schema["required"].as_array().unwrap();
        assert!(required.iter().any(|value| value == "path"), "{required:?}");

        let screen_spec = registry.get("computer.read_screen").unwrap().spec();
        assert_eq!(screen_spec.name, "computer.read_screen");
        assert_eq!(screen_spec.schema["type"], "object");
        assert!(screen_spec.schema["properties"].is_object());
        assert!(
            screen_spec.schema.get("required").is_none(),
            "read_screen takes no arguments"
        );
    }

    #[test]
    fn the_schemas_document_what_the_tools_need() {
        let registry = ToolRegistry::with_builtins();
        let click_spec = registry.get("computer.click").unwrap().spec();
        let required = click_spec.schema["required"].as_array().unwrap();
        for field in ["x", "y"] {
            assert!(required.iter().any(|value| value == field), "{field}");
        }
        let scroll_spec = registry.get("computer.scroll").unwrap().spec();
        let directions = scroll_spec.schema["properties"]["direction"]["enum"]
            .as_array()
            .unwrap();
        assert_eq!(directions.len(), 2, "direction enum: {directions:?}");
        assert_eq!(directions[0], json!("up"));
        assert_eq!(directions[1], json!("down"));
        for name in ["computer.windows", "computer.status"] {
            let spec = registry.get(name).unwrap().spec();
            assert_eq!(spec.schema["type"], "object");
            assert!(spec.schema["properties"].is_object());
        }
    }

    #[test]
    fn status_runs_without_requiring_any_helper() {
        let out = Status.run(&json!({}), &ctx()).unwrap();
        assert!(out.ok, "{}", out.content);
        assert!(out.content.contains("detected:"), "{}", out.content);
        assert!(out.content.contains("screenshot:"), "{}", out.content);
        assert!(out.content.contains("input:"), "{}", out.content);
        assert!(out.content.contains("ocr:"), "{}", out.content);
        assert!(out.content.contains("session:"), "{}", out.content);
        // The two helpers added for OCR and Wayland input are reported by name, present or not.
        for helper in ["tesseract", "ydotool"] {
            assert!(out.content.contains(helper), "{helper}: {}", out.content);
        }
    }

    #[test]
    fn tool_run_reports_refusals_as_failed_output_not_errors() {
        // Every one of these is refused before a helper is spawned, so no desktop is needed.
        let out = Click.run(&json!({"x": -1, "y": 0}), &ctx()).unwrap();
        assert!(!out.ok);
        assert!(out.content.contains("non-negative"), "{}", out.content);

        let out = TypeText.run(&json!({"text": ""}), &ctx()).unwrap();
        assert!(!out.ok);

        let out = Key.run(&json!({"keys": "-x"}), &ctx()).unwrap();
        assert!(!out.ok);

        let out = Scroll
            .run(&json!({"direction": "diagonal", "amount": 2}), &ctx())
            .unwrap();
        assert!(!out.ok);
        assert!(out.content.contains("up"), "{}", out.content);
    }

    #[test]
    fn a_tool_run_without_its_arguments_is_a_provider_rejection() {
        let err = Screenshot.run(&json!({}), &ctx()).unwrap_err();
        assert!(matches!(err, VikramError::ProviderRejected(_)), "{err:?}");
        let err = Click.run(&json!({"y": 1}), &ctx()).unwrap_err();
        assert!(matches!(err, VikramError::ProviderRejected(_)), "{err:?}");
        let err = Key.permission(&json!({})).unwrap_err();
        assert!(matches!(err, VikramError::ProviderRejected(_)), "{err:?}");
        let err = Ocr.run(&json!({}), &ctx()).unwrap_err();
        assert!(matches!(err, VikramError::ProviderRejected(_)), "{err:?}");
        let err = Ocr.permission(&json!({})).unwrap_err();
        assert!(matches!(err, VikramError::ProviderRejected(_)), "{err:?}");
        // A path that is present but not a string is a rejection, not a silent empty path.
        let err = Ocr.run(&json!({"path": 7}), &ctx()).unwrap_err();
        assert!(matches!(err, VikramError::ProviderRejected(_)), "{err:?}");
        // `read_screen` needs no arguments, so its permission shape never rejects.
        assert!(ReadScreen.permission(&json!({})).is_ok());
    }

    #[test]
    fn screenshot_permission_names_the_requested_path() {
        let registry = ToolRegistry::with_builtins();
        let req = registry
            .get("computer.screenshot")
            .unwrap()
            .permission(&json!({"path": "shots/a.png"}))
            .unwrap();
        assert_eq!(req.resource, "shots/a.png");
    }
}
