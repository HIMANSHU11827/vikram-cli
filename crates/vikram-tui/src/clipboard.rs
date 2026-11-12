//! Best-effort access to the system clipboard.
//!
//! The interface never depends on a clipboard being present: every operation is a no-op when
//! `arboard` cannot reach the platform clipboard (headless sessions, missing display server, ...).
//! Callers must therefore treat a missing result as "nothing available" rather than as an error.

/// Copy `text` to the system clipboard when one is reachable.
pub fn copy(text: &str) {
    if text.is_empty() {
        return;
    }
    if let Ok(mut clipboard) = arboard::Clipboard::new() {
        let _ = clipboard.set_text(text.to_owned());
    }
}

/// Read text from the system clipboard when one is reachable.
pub fn paste() -> Option<String> {
    let mut clipboard = arboard::Clipboard::new().ok()?;
    clipboard.get_text().ok()
}
