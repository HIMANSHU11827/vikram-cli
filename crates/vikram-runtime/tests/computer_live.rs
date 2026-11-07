//! Live desktop tests for the [`computer`] module: these drive the helper programs that are really
//! installed on the machine (`import`/`grim` for screenshots, `tesseract` for OCR) rather than a
//! mock, and skip with a printed reason when a helper is absent so a headless box stays green.
//!
//! Two rules keep the suite safe to run on a developer's desktop:
//!
//! * **Nothing is typed, clicked or scrolled.** The input helpers (`xdotool`) are not installed
//!   where these tests were written, so the input path is covered by asserting the refusal it
//!   produces — the message that names the missing tool and its install hint.
//! * **No wedged helper can hang the run.** Every call into the module goes through [`within`],
//!   which abandons a call that outlives its budget; the module itself also kills each helper after
//!   [`TIMEOUT_SECS`] / [`OCR_TIMEOUT_SECS`].
//!
//! `PATH` is the only environment input the module's resolver reads, so the one test that scrubs it
//! takes the file-wide [`PATH_LOCK`] and restores the previous value on the way out; libtest runs
//! the tests in this binary on several threads, and a test that saw the scrubbed value would fail
//! for a reason that has nothing to do with the code under test.

use std::ffi::OsString;
use std::path::{Path, PathBuf};
use std::process::Command;
use std::sync::{Mutex, MutexGuard};
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use vikram_runtime::computer::{
    self, OCR_TIMEOUT_SECS, TIMEOUT_SECS, available, missing_tool_message, require_tool, status,
    which,
};

/// The text the OCR test renders and then expects `tesseract` to read back.
const RENDERED_TEXT: &str = "VIKRAM OCR 12345";

/// A name no package manager, `PATH` entry or fixture provides: the resolver must refuse it.
const ABSENT: &str = "definitely-not-a-real-tool";

/// Serialises every test in this binary that reads or mutates the process `PATH`.
static PATH_LOCK: Mutex<()> = Mutex::new(());

fn path_lock() -> MutexGuard<'static, ()> {
    PATH_LOCK.lock().unwrap_or_else(|err| err.into_inner())
}

/// A unique scratch directory that removes itself, so a failed assertion leaves no files behind.
struct Scratch(PathBuf);

impl Scratch {
    fn new(tag: &str) -> Self {
        let stamp = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .map(|elapsed| elapsed.as_nanos())
            .unwrap_or(0);
        let dir = std::env::temp_dir().join(format!(
            "vikram-computer-live-{tag}-{}-{stamp}",
            std::process::id()
        ));
        std::fs::create_dir_all(&dir).expect("create scratch directory");
        Self(dir)
    }

    fn join(&self, name: &str) -> PathBuf {
        self.0.join(name)
    }
}

impl Drop for Scratch {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.0);
    }
}

/// `PATH` pointed at an empty directory for as long as this guard lives.
///
/// The previous value is put back in [`Drop`], *before* the lock is released (the guard field is
/// dropped after this type's own `drop` body runs), so no other test in this binary can observe the
/// scrubbed value.
struct ScrubbedPath {
    _guard: MutexGuard<'static, ()>,
    saved: Option<OsString>,
}

impl ScrubbedPath {
    fn new(dir: &Path) -> Self {
        let guard = path_lock();
        let saved = std::env::var_os("PATH");
        // SAFETY: `PATH` is process-global state and `std::env::set_var` is unsafe in the 2024
        // edition for that reason. Every test here that reads `PATH` holds `PATH_LOCK`, and this
        // guard holds it until the value has been restored in `drop`.
        unsafe { std::env::set_var("PATH", dir) };
        Self {
            _guard: guard,
            saved,
        }
    }
}

impl Drop for ScrubbedPath {
    fn drop(&mut self) {
        // SAFETY: as above; the lock is still held here.
        unsafe {
            match self.saved.take() {
                Some(saved) => std::env::set_var("PATH", saved),
                None => std::env::remove_var("PATH"),
            }
        }
    }
}

/// Run `work` on its own thread and give up once `budget` is spent.
///
/// The worker is detached on purpose: a helper that ignored the module's kill signal is not worth
/// joining, and the suite must not wait on it.
fn within<T: Send + 'static>(
    what: &str,
    budget: Duration,
    work: impl FnOnce() -> T + Send + 'static,
) -> T {
    let (tx, rx) = std::sync::mpsc::channel();
    std::thread::spawn(move || {
        let _ = tx.send(work());
    });
    match rx.recv_timeout(budget) {
        Ok(value) => value,
        Err(_) => panic!("{what} did not finish within {budget:?}"),
    }
}

/// The reading budget for one module call: the module's own helper limit plus slack for the
/// screenshot itself, which may capture a 4K screen.
fn helper_budget(module_limit_secs: u64) -> Duration {
    Duration::from_secs(module_limit_secs + 20)
}

fn be_u32(bytes: &[u8], at: usize) -> u32 {
    u32::from_be_bytes([bytes[at], bytes[at + 1], bytes[at + 2], bytes[at + 3]])
}

/// Assert `path` is a PNG and return the `IHDR` dimensions, so "a file appeared" cannot pass for
/// "an image was captured".
fn png_dimensions(path: &Path) -> (u32, u32) {
    let bytes = std::fs::read(path).expect("read the captured PNG");
    assert!(
        bytes.starts_with(b"\x89PNG\r\n\x1a\n"),
        "{} is not a PNG ({} bytes, starts {:?})",
        path.display(),
        bytes.len(),
        &bytes[..bytes.len().min(8)]
    );
    assert!(bytes.len() > 33, "PNG truncated: {} bytes", bytes.len());
    assert_eq!(&bytes[12..16], b"IHDR", "first PNG chunk is not IHDR");
    let width = be_u32(&bytes, 16);
    let height = be_u32(&bytes, 20);
    assert!(
        width > 0 && height > 0,
        "PNG reports an empty frame: {width}x{height}"
    );
    (width, height)
}

/// The session line from [`status`], e.g. `wayland`.
fn session_report() -> String {
    status()
        .lines()
        .find(|line| line.starts_with("session: "))
        .unwrap_or_default()
        .to_string()
}

// --- 1. detection against the real PATH --------------------------------------------------------

#[test]
fn status_and_available_reflect_the_helpers_actually_installed() {
    let _env = path_lock();

    let detected = available();
    eprintln!("available(): {detected:?}");

    // This machine has ImageMagick and tesseract; neither is optional for this test run.
    for helper in ["import", "tesseract"] {
        assert!(
            which(helper).is_some(),
            "`{helper}` must be on PATH for the live computer tests"
        );
        assert!(
            detected.contains(&helper),
            "available() must report `{helper}`: {detected:?}"
        );
    }
    // ... and xdotool is the one helper this machine does not have.
    assert!(
        which("xdotool").is_none(),
        "xdotool is installed; the input tests expect it to be absent"
    );
    assert!(
        !detected.contains(&"xdotool"),
        "available() must not report xdotool: {detected:?}"
    );

    let report = status();
    eprintln!("status():\n{report}");

    let detected_line = report
        .lines()
        .find(|line| line.starts_with("detected: "))
        .expect("status must report what it detected");
    let names: Vec<&str> = detected_line
        .trim_start_matches("detected: ")
        .split(',')
        .map(str::trim)
        .collect();
    eprintln!("detected line: {names:?}");
    assert!(names.contains(&"import"), "{detected_line}");
    assert!(names.contains(&"tesseract"), "{detected_line}");
    assert!(!names.contains(&"xdotool"), "{detected_line}");

    // The absent helper is not silently dropped: it is listed with its install hint.
    let missing_line = report
        .lines()
        .find(|line| line.starts_with("missing: "))
        .expect("status must list the helpers it could not find");
    assert!(
        missing_line.contains("xdotool (apt install xdotool)"),
        "{missing_line}"
    );

    // The capability lines agree with detection rather than with the documentation. Which capture
    // helper is *chosen* follows the session (grim on Wayland, scrot/import on X11), so pin the
    // property that has to hold either way: a screenshot is possible, through a helper that was
    // actually detected.
    let screenshot_line = report
        .lines()
        .find(|line| line.starts_with("screenshot: "))
        .expect("status must report the screenshot helper");
    let chosen = screenshot_line.trim_start_matches("screenshot: ");
    eprintln!("screenshot helper: {chosen}");
    assert!(
        ["scrot", "import", "grim"].contains(&chosen),
        "{screenshot_line}"
    );
    assert!(detected.contains(&chosen), "{screenshot_line}");
    assert!(
        report.contains("\nocr: tesseract (computer.ocr, computer.read_screen)"),
        "{report}"
    );
    assert!(
        report.contains("\ninput: unavailable (no xdotool"),
        "{report}"
    );
    assert!(
        report.contains("\nwindows: unavailable (no wmctrl or xdotool)"),
        "{report}"
    );

    // A display is configured on this machine, so the report must not claim to be headless.
    assert!(
        std::env::var_os("DISPLAY").is_some() || std::env::var_os("WAYLAND_DISPLAY").is_some(),
        "neither DISPLAY nor WAYLAND_DISPLAY is set; the rest of the suite needs a display"
    );
    assert!(!report.contains("headless"), "{report}");
}

// --- 2. a real screen capture ------------------------------------------------------------------

#[test]
fn a_live_screenshot_is_a_real_png_with_a_positive_size() {
    let _env = path_lock();

    if which("import").is_none() {
        eprintln!("skipping live screenshot test: `import` (ImageMagick) is not installed");
        return;
    }
    if std::env::var_os("DISPLAY").is_none() {
        eprintln!("skipping live screenshot test: DISPLAY is not set");
        return;
    }

    let scratch = Scratch::new("screenshot");
    let target = scratch.join("screen.png");
    let requested = target.to_string_lossy().into_owned();
    let call = requested.clone();

    let message = within(
        "computer::screenshot",
        helper_budget(TIMEOUT_SECS),
        move || computer::screenshot(&call),
    )
    .unwrap_or_else(|err| panic!("screenshot of the live display failed: {err}"));
    eprintln!("screenshot(): {message}");

    assert!(
        message.contains(&requested),
        "the report must name the file it wrote: {message}"
    );
    assert!(
        message.contains("via "),
        "the report must name the helper used: {message}"
    );
    assert!(target.is_file(), "no file was written at {requested}");

    let (width, height) = png_dimensions(&target);
    eprintln!("captured PNG: {width}x{height} at {requested}");
    assert!(
        width >= 100 && height >= 100,
        "a full-screen capture should be larger than {width}x{height}"
    );

    // ImageMagick must agree with the bytes we parsed, when it is installed to ask.
    if which("identify").is_some() {
        let identified = Command::new("identify")
            .arg(&target)
            .output()
            .expect("run identify");
        let text = String::from_utf8_lossy(&identified.stdout).into_owned();
        eprintln!("identify: {}", text.trim());
        assert!(
            identified.status.success() && text.contains(&format!("PNG {width}x{height}")),
            "identify disagrees with the IHDR we parsed: {text}"
        );
    }
}

// --- 3. OCR over an image this test rendered ----------------------------------------------------

#[test]
fn ocr_reads_text_from_an_image_rendered_by_imagemagick() {
    let _env = path_lock();

    if which("convert").is_none() {
        eprintln!("skipping live OCR test: `convert` (ImageMagick) is not installed");
        return;
    }
    if which("tesseract").is_none() {
        eprintln!("skipping live OCR test: `tesseract` is not installed");
        return;
    }

    let scratch = Scratch::new("ocr");
    let image = scratch.join("text.png");
    let rendered = Command::new("convert")
        .args([
            "-size",
            "600x120",
            "xc:white",
            "-pointsize",
            "48",
            "-fill",
            "black",
            "-annotate",
            "+20+70",
            RENDERED_TEXT,
        ])
        .arg(&image)
        .output()
        .expect("run convert to render the OCR fixture");
    assert!(
        rendered.status.success(),
        "convert failed: {}",
        String::from_utf8_lossy(&rendered.stderr).trim()
    );
    assert_eq!(
        png_dimensions(&image),
        (600, 120),
        "the fixture is not the image convert was asked for"
    );

    let path = image.to_string_lossy().into_owned();
    let call = path.clone();
    let text = within(
        "computer::ocr",
        helper_budget(OCR_TIMEOUT_SECS),
        move || computer::ocr(&call),
    )
    .unwrap_or_else(|err| panic!("ocr of {path} failed: {err}"));
    eprintln!("ocr() read: {text:?}");

    // OCR may re-case and re-space, so compare a whitespace-free, caseless form.
    let compact: String = text.chars().filter(|ch| !ch.is_whitespace()).collect();
    let lowered = compact.to_lowercase();
    assert!(
        compact.contains("12345"),
        "the digits must come back from `{RENDERED_TEXT}`: {text:?}"
    );
    assert!(
        lowered.contains("vikram"),
        "the word VIKRAM must come back from `{RENDERED_TEXT}`: {text:?}"
    );
    assert!(
        lowered.contains("ocr"),
        "the word OCR must come back from `{RENDERED_TEXT}`: {text:?}"
    );
}

// --- 4. what happens when nothing is on PATH ----------------------------------------------------

#[test]
fn an_absent_helper_and_an_empty_path_both_explain_how_to_install_it() {
    // The absent-name route needs no global state at all, so it runs first and unguarded.
    let err = require_tool(ABSENT).expect_err("a made-up helper name must not resolve");
    eprintln!("require_tool({ABSENT}): {err}");
    assert!(err.contains(ABSENT), "{err}");
    assert!(err.contains("not found"), "{err}");
    assert!(err.contains("install it"), "{err}");
    assert!(err.contains("PATH"), "{err}");
    assert_eq!(err, missing_tool_message(ABSENT));

    // `which` resolves an absolute path as given, and only accepts an executable file.
    assert!(which("/bin/sh").is_some(), "`which` must find /bin/sh");
    assert!(
        which("/etc/hostname").is_none(),
        "`which` must not accept a non-executable file"
    );
    assert!(which("").is_none(), "an empty name must not resolve");
    assert!(which("   ").is_none(), "a blank name must not resolve");

    // The same failure, produced the way it happens in the field: nothing on PATH at all.
    let scratch = Scratch::new("empty-path");
    let empty_bin = scratch.join("bin");
    std::fs::create_dir_all(&empty_bin).expect("create the empty PATH directory");

    let saved;
    {
        let scrub = ScrubbedPath::new(&empty_bin);
        saved = scrub.saved.clone();
        assert_eq!(
            std::env::var_os("PATH").as_deref(),
            Some(empty_bin.as_os_str()),
            "the scrub did not take effect"
        );

        assert!(
            which("import").is_none(),
            "`import` must not resolve on an empty PATH"
        );
        assert!(
            available().is_empty(),
            "nothing is available on an empty PATH: {:?}",
            available()
        );

        let err = require_tool("tesseract").expect_err("tesseract must not resolve");
        eprintln!("with an empty PATH, require_tool(tesseract): {err}");
        assert!(err.contains("tesseract not found"), "{err}");
        assert!(err.contains("apt install tesseract-ocr"), "{err}");

        let target = scratch.join("nothing.png");
        let err = computer::screenshot(&target.to_string_lossy())
            .expect_err("a screenshot cannot be taken without a helper");
        eprintln!("with an empty PATH, screenshot(): {err}");
        assert!(err.contains("no screenshot helper found"), "{err}");
        assert!(err.contains("apt install imagemagick"), "{err}");
        assert!(!target.exists(), "a failed screenshot wrote a file anyway");

        let image = scratch.join("nothing-ocr.png");
        let err =
            computer::ocr(&image.to_string_lossy()).expect_err("OCR cannot run without tesseract");
        assert!(err.contains("tesseract not found"), "{err}");
        assert!(err.contains("apt install tesseract-ocr"), "{err}");

        let report = status();
        eprintln!("status() with an empty PATH:\n{report}");
        assert!(report.contains("detected: none"), "{report}");
        assert!(
            report.contains("screenshot: unavailable (no scrot, import or grim)"),
            "{report}"
        );
        assert!(
            report.contains("ocr: unavailable (no tesseract)"),
            "{report}"
        );
    }

    // Restored, not just unlocked: the next test in this binary sees the machine's real PATH.
    assert_eq!(
        std::env::var_os("PATH"),
        saved,
        "the original PATH was not restored"
    );
    assert!(
        which("import").is_some(),
        "`import` must resolve again once PATH is restored"
    );
}

// --- 5. input helpers this machine does not have ------------------------------------------------

#[test]
fn click_type_and_key_name_the_missing_input_helper() {
    let _env = path_lock();

    if which("xdotool").is_some() {
        eprintln!(
            "skipping the live input test: xdotool is installed, so a real click would be sent to \
             this desktop"
        );
        return;
    }
    // On a Wayland session with ydotool present, click/type go through ydotool instead and this
    // refusal never happens; do not pretend to cover that path here.
    let session = session_report();
    eprintln!("{session}");
    if session.contains("wayland") && which("ydotool").is_some() {
        eprintln!(
            "skipping the live input test: this Wayland session routes input through ydotool"
        );
        return;
    }

    let started = std::time::Instant::now();
    let click_err = computer::click(10, 20, 1).expect_err("click cannot work without xdotool");
    let type_err = computer::type_text("vikram").expect_err("type cannot work without xdotool");
    let key_err = computer::key("ctrl+l").expect_err("key cannot work without xdotool");
    let scroll_err = computer::scroll("down", 1).expect_err("scroll cannot work without xdotool");
    // Absence is reported, not waited out: four refusals must be far below one helper timeout.
    assert!(
        started.elapsed() < Duration::from_secs(TIMEOUT_SECS),
        "refusing four input calls took {:?}",
        started.elapsed()
    );
    eprintln!("click:  {click_err}");
    eprintln!("type:   {type_err}");
    eprintln!("key:    {key_err}");
    eprintln!("scroll: {scroll_err}");

    for (what, err) in [
        ("click", &click_err),
        ("type", &type_err),
        ("key", &key_err),
        ("scroll", &scroll_err),
    ] {
        assert!(err.contains("xdotool"), "{what} must name xdotool: {err}");
        assert!(
            err.contains("apt install xdotool"),
            "{what} must carry the install hint: {err}"
        );
        assert!(
            err.contains("not found") || err.contains("needs xdotool"),
            "{what} must say why it could not run: {err}"
        );
    }
    assert_eq!(click_err, missing_tool_message("xdotool"));
    assert_eq!(type_err, missing_tool_message("xdotool"));
    assert_eq!(key_err, missing_tool_message("xdotool"));
    assert_eq!(scroll_err, missing_tool_message("xdotool"));

    // The window listing has the same shape of answer when neither helper exists.
    if which("wmctrl").is_none() && which("xdotool").is_none() {
        let err = computer::windows().expect_err("no helper can list windows here");
        eprintln!("windows: {err}");
        assert!(err.contains("no window listing helper found"), "{err}");
        assert!(err.contains("apt install wmctrl"), "{err}");
        assert!(err.contains("apt install xdotool"), "{err}");
    } else {
        eprintln!("skipping the window-listing assertion: a window helper is installed");
    }
}
