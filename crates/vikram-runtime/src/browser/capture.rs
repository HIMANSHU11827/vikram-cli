use std::io::Read;
use std::path::Path;
use std::process::{Command, Stdio};
use std::sync::mpsc::{self, Receiver};
use std::time::{Duration, Instant};

/// How often the child's exit is polled while a browser run is in flight.
const POLL_INTERVAL: Duration = Duration::from_millis(20);

/// How long the child's pipes are given to drain once it has exited or been killed.
const DRAIN_TIMEOUT: Duration = Duration::from_millis(500);

/// How many of the browser's last stderr lines are quoted in an error message.
const STDERR_LINES: usize = 6;

/// How many characters of that stderr tail are kept.
const STDERR_CHARS: usize = 600;

/// What one browser run produced.
struct Capture {
    stdout: Vec<u8>,
    stderr: String,
    code: Option<i32>,
    success: bool,
}

/// Load `url` in headless mode and return the serialized DOM Chrome built.
///
/// The DOM is *post-script*: whatever the page's own JavaScript produced is included, which is the
/// whole point of doing this instead of an HTTP GET.
pub fn navigate(url: &str, timeout_ms: u64) -> Result<String, String> {
    let bin = available().ok_or_else(|| MISSING_BROWSER.to_string())?;
    navigate_with(&bin, url, timeout_ms)
}

/// [`navigate`] against a named binary, so the failure paths can be exercised without one.
pub fn navigate_with(bin: &Path, url: &str, timeout_ms: u64) -> Result<String, String> {
    let args = browser_args(&["--dump-dom"], url);
    let capture = run(bin, &args, timeout_ms)?;
    if !capture.success {
        return Err(format!(
            "`{}` exited with {} while loading {url}{}",
            bin.display(),
            describe_code(capture.code),
            describe_stderr(&capture.stderr)
        ));
    }
    let dom = String::from_utf8_lossy(&capture.stdout).into_owned();
    if dom.trim().is_empty() {
        return Err(format!(
            "`{}` produced no DOM for {url}{}",
            bin.display(),
            describe_stderr(&capture.stderr)
        ));
    }
    Ok(dom)
}

/// Render `url` to `out_path` and return the path that was written.
///
/// The file is checked afterwards: a browser that exits successfully without writing anything is
/// reported as a failure rather than as a path that does not exist.
pub fn screenshot(url: &str, out_path: &Path, width: u32, height: u32) -> Result<String, String> {
    let bin = available().ok_or_else(|| MISSING_BROWSER.to_string())?;
    screenshot_with(&bin, url, out_path, width, height, DEFAULT_TIMEOUT_MS)
}

/// [`screenshot`] against a named binary with an explicit timeout.
pub fn screenshot_with(
    bin: &Path,
    url: &str,
    out_path: &Path,
    width: u32,
    height: u32,
    timeout_ms: u64,
) -> Result<String, String> {
    let width = width.clamp(1, MAX_DIMENSION);
    let height = height.clamp(1, MAX_DIMENSION);
    if let Some(parent) = out_path.parent() {
        if !parent.as_os_str().is_empty() {
            std::fs::create_dir_all(parent)
                .map_err(|e| format!("cannot create `{}`: {e}", parent.display()))?;
        }
    }
    let shot = format!("--screenshot={}", out_path.display());
    let window = format!("--window-size={width},{height}");
    let args = browser_args(&[&shot, &window], url);
    let capture = run(bin, &args, timeout_ms)?;
    if !capture.success {
        return Err(format!(
            "`{}` exited with {} while capturing {url}{}",
            bin.display(),
            describe_code(capture.code),
            describe_stderr(&capture.stderr)
        ));
    }
    if !out_path.is_file() {
        return Err(format!(
            "`{}` exited successfully but wrote no screenshot to `{}`{}",
            bin.display(),
            out_path.display(),
            describe_stderr(&capture.stderr)
        ));
    }
    Ok(out_path.display().to_string())
}

/// The headless flags every run shares, plus `extra`, plus the URL last.
pub(crate) fn browser_args(extra: &[&str], url: &str) -> Vec<String> {
    let mut args: Vec<String> = vec![
        "--headless=new".to_string(),
        "--disable-gpu".to_string(),
        "--no-sandbox".to_string(),
    ];
    args.extend(extra.iter().map(|arg| (*arg).to_string()));
    args.push(url.to_string());
    args
}

/// Spawn the browser, wait for it under `timeout_ms`, and collect both pipes.
fn run(bin: &Path, args: &[String], timeout_ms: u64) -> Result<Capture, String> {
    let mut child = Command::new(bin)
        .args(args)
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .map_err(|e| format!("cannot start `{}`: {e}", bin.display()))?;

    let stdout = child.stdout.take().map(read_in_background);
    let stderr = child.stderr.take().map(read_in_background);

    let timeout = Duration::from_millis(timeout_ms.max(1));
    let deadline = Instant::now() + timeout;
    let mut timed_out = false;
    let status = loop {
        match child.try_wait() {
            Ok(Some(status)) => break Some(status),
            Ok(None) => {
                if Instant::now() >= deadline {
                    timed_out = true;
                    let _ = child.kill();
                    let _ = child.wait();
                    break None;
                }
                std::thread::sleep(POLL_INTERVAL);
            }
            Err(e) => {
                let _ = child.kill();
                let _ = child.wait();
                return Err(format!("failed while waiting for `{}`: {e}", bin.display()));
            }
        }
    };

    let stdout = stdout.map(collect).unwrap_or_default();
    let stderr = String::from_utf8_lossy(&stderr.map(collect).unwrap_or_default()).into_owned();

    if timed_out {
        return Err(format!(
            "`{}` did not finish within {timeout_ms} ms and was killed{}",
            bin.display(),
            describe_stderr(&stderr)
        ));
    }

    Ok(Capture {
        stdout,
        stderr,
        code: status.and_then(|status| status.code()),
        success: status.map(|status| status.success()).unwrap_or(false),
    })
}

/// Read a pipe to the end on its own thread.
///
/// A thread rather than a blocking read on the main thread: a chatty page must not fill the pipe
/// buffer and deadlock the child while the exit is being polled. A subprocess that outlives its
/// parent's kill can keep such a thread alive; it owns nothing but that pipe, so it is left to end
/// on its own rather than joined.
fn read_in_background<R: Read + Send + 'static>(mut pipe: R) -> Receiver<Vec<u8>> {
    let (tx, rx) = mpsc::channel();
    let _ = std::thread::spawn(move || {
        let mut buffer = Vec::new();
        let _ = pipe.read_to_end(&mut buffer);
        let _ = tx.send(buffer);
    });
    rx
}

/// Collect a background read, giving up after [`DRAIN_TIMEOUT`].
fn collect(rx: Receiver<Vec<u8>>) -> Vec<u8> {
    rx.recv_timeout(DRAIN_TIMEOUT).unwrap_or_default()
}

/// Quote the interesting part of the browser's stderr, or nothing at all.
pub(crate) fn describe_stderr(stderr: &str) -> String {
    let lines: Vec<&str> = stderr
        .lines()
        .map(str::trim)
        .filter(|line| !line.is_empty())
        .collect();
    if lines.is_empty() {
        return String::new();
    }
    let start = lines.len().saturating_sub(STDERR_LINES);
    let mut tail = lines[start..].join(" | ");
    if tail.chars().count() > STDERR_CHARS {
        tail = tail.chars().take(STDERR_CHARS).collect::<String>() + "…";
    }
    format!(": {tail}")
}

pub(crate) fn describe_code(code: Option<i32>) -> String {
    match code {
        Some(code) => code.to_string(),
        None => "a signal".to_string(),
    }
}

// ---------------------------------------------------------------------------------------------
// Reading the DOM
// ---------------------------------------------------------------------------------------------

use crate::browser::*;
