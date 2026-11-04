//! DOM-aware browsing through an installed Chromium-family binary.
//!
//! [`crate::tools::BrowserFetch`] is a plain HTTP GET: it sees the bytes the server sent, not the
//! document the browser builds. This module fills that gap by driving a headless Chromium/Chrome/
//! Edge with `--dump-dom` and `--screenshot`, so a caller can read the *rendered* page — its text,
//! its links and its form controls — without adding an HTTP/DOM dependency to the crate.
//!
//! Three properties are deliberate:
//!
//! * **Nothing is installed.** Discovery looks at `VIKRAM_CHROMIUM`, then at the usual program
//!   names on `PATH`, then at the usual absolute locations. A machine with no browser reports
//!   [`MISSING_BROWSER`] and names the variable to set.
//! * **No shell.** The browser is spawned with an argv vector, so a URL cannot turn into a second
//!   command.
//! * **Bounded.** Every run is killed after the caller's timeout, and the child's pipes are drained
//!   on a short deadline so a wedged browser cannot wedge the turn that asked for the page.
//!
//! The second half of the module is interactive: [`cdp::open`] keeps a headless browser alive behind
//! the Chrome DevTools Protocol, so a page that only reveals itself after a click or a keystroke can
//! be driven. Interaction goes through CDP's `Input` domain — a real mouse press and release at the
//! element's own centre, `Input.insertText` for typing, a wheel event for scrolling — so the page
//! receives the same *trusted* events a user's hands would have produced. That distinction is not
//! cosmetic: `event.isTrusted` is false for a scripted `.click()`, React and Vue never see a
//! hand-assigned `.value` (their change trackers compare against the last value *they* wrote), and a
//! `<canvas>` reacts to nothing but a pointer event. Script still does three jobs here — measuring an
//! element's box, scrolling it into view, focusing it — none of which is an event, so no page can
//! tell the difference between that and a layout pass.
//!
//! What neither half is: there is no vision. Nothing here can see a screenshot, locate an element by
//! looking at it, or wait for an element to appear beyond the load poll `open` performs.

use std::collections::HashMap;
use std::io::{Read, Write};
use std::net::{SocketAddr, TcpStream};
use std::path::{Path, PathBuf};
use std::process::{Child, Command, Stdio};
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Mutex, OnceLock};
use std::time::{Duration, Instant, SystemTime};

use serde_json::{Value, json};
use tungstenite::WebSocket;

/// The environment variable that overrides browser discovery, and that every error names.
pub const ENV_VAR: &str = "VIKRAM_CHROMIUM";

/// The error reported when no Chromium-family binary can be found.
pub const MISSING_BROWSER: &str = "no Chromium/Chrome found; set VIKRAM_CHROMIUM to its path";

/// How long a browser run may take when the caller does not say otherwise.
pub const DEFAULT_TIMEOUT_MS: u64 = 30_000;

/// Screenshots wider or taller than this are clamped; the value keeps a typo from asking for a
/// 40 GB image.
pub const MAX_DIMENSION: u32 = 10_000;

pub use crate::browser::capture::*;
pub use crate::browser::cdp::*;
pub use crate::browser::discovery::*;
pub use crate::browser::dom::*;

pub(crate) mod capture;
/// Interactive browsing: a browser that stays alive between calls.
///
/// The split from the one-shot [`crate::browser::navigate`] path is deliberate. A one-shot run must
/// be *hermetic*: launch, render, exit. Interaction cannot be — the page must survive until the next
/// click — so a session owns a child process, a WebSocket to its DevTools endpoint, and its profile
/// directory, and releases all three in [`cdp::close`] or when the process ends.
pub mod cdp;
pub(crate) mod discovery;
pub(crate) mod dom;

#[cfg(test)]
mod tests;
