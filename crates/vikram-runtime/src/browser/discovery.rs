use std::path::{Path, PathBuf};

/// Program names looked up on `PATH`, in order.
const PATH_CANDIDATES: &[&str] = &[
    "chromium",
    "chromium-browser",
    "google-chrome",
    "google-chrome-stable",
    "msedge",
];

/// Absolute locations checked when no candidate is on `PATH`.
const ABSOLUTE_CANDIDATES: &[&str] = &[
    "/usr/bin/chromium",
    "/usr/bin/chromium-browser",
    "/usr/bin/google-chrome",
    "/usr/bin/google-chrome-stable",
    "/bin/chromium",
    "/snap/bin/chromium",
    "/snap/bin/google-chrome",
    "/usr/lib/chromium/chromium",
    "/opt/google/chrome/google-chrome",
    "/opt/google/chrome/chrome",
    "/opt/microsoft/msedge/msedge",
    "/Applications/Google Chrome.app/Contents/MacOS/Google Chrome",
    "/Applications/Chromium.app/Contents/MacOS/Chromium",
    "/Applications/Microsoft Edge.app/Contents/MacOS/Microsoft Edge",
    "C:\\Program Files\\Google\\Chrome\\Application\\chrome.exe",
    "C:\\Program Files (x86)\\Google\\Chrome\\Application\\chrome.exe",
    "C:\\Program Files\\Microsoft\\Edge\\Application\\msedge.exe",
    "C:\\Program Files (x86)\\Microsoft\\Edge\\Application\\msedge.exe",
];

/// The Chromium-family binary this machine has, or `None` when it has none.
///
/// `VIKRAM_CHROMIUM` wins when it names a file (or a program on `PATH`); a stale override falls
/// through to automatic discovery rather than disabling browsing for the whole session.
pub fn available() -> Option<PathBuf> {
    resolve(
        |key| std::env::var(key).ok(),
        find_on_path,
        |path| path.is_file(),
    )
}

/// A human description of the browser in use, for tool output.
pub fn describe_available() -> String {
    match available() {
        Some(path) => path.display().to_string(),
        None => MISSING_BROWSER.to_string(),
    }
}

/// Discovery with its three inputs injected, so the ordering can be tested without a browser.
pub(crate) fn resolve(
    env: impl Fn(&str) -> Option<String>,
    on_path: impl Fn(&str) -> Option<PathBuf>,
    is_file: impl Fn(&Path) -> bool,
) -> Option<PathBuf> {
    if let Some(raw) = env(ENV_VAR) {
        let raw = raw.trim();
        if !raw.is_empty() {
            let candidate = PathBuf::from(raw);
            let looks_like_path =
                candidate.is_absolute() || raw.contains('/') || raw.contains('\\');
            if looks_like_path {
                if is_file(&candidate) {
                    return Some(candidate);
                }
            } else if let Some(found) = on_path(raw) {
                return Some(found);
            }
        }
    }
    for name in PATH_CANDIDATES {
        if let Some(found) = on_path(name) {
            return Some(found);
        }
    }
    for candidate in ABSOLUTE_CANDIDATES {
        let path = PathBuf::from(candidate);
        if is_file(&path) {
            return Some(path);
        }
    }
    None
}

/// Look a program up on `PATH` without a shell.
fn find_on_path(name: &str) -> Option<PathBuf> {
    let path = std::env::var_os("PATH")?;
    for dir in std::env::split_paths(&path) {
        if dir.as_os_str().is_empty() {
            continue;
        }
        let direct = dir.join(name);
        if direct.is_file() {
            return Some(direct);
        }
        #[cfg(windows)]
        {
            let exe = dir.join(format!("{name}.exe"));
            if exe.is_file() {
                return Some(exe);
            }
        }
    }
    None
}

// ---------------------------------------------------------------------------------------------
// Running the browser
// ---------------------------------------------------------------------------------------------

use crate::browser::*;
