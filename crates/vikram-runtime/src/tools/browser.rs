use serde_json::{Value, json};
use std::time::Duration;
use vikram_contracts::{ActionKind, PermissionRequest, SideEffect, Trust, VikramError};

use crate::tools::*;
pub struct BrowserFetch;

impl Tool for BrowserFetch {
    fn spec(&self) -> ToolSpec {
        ToolSpec {
            name: "browser.fetch".into(),
            description: "Fetch a URL and return its content.".into(),
            schema: json!({
                "type": "object",
                "properties": {
                    "url": {"type": "string"},
                    "max_length": {"type": "integer"}
                },
                "required": ["url"]
            }),
        }
    }

    fn permission(&self, args: &Value) -> Result<PermissionRequest, VikramError> {
        let url = arg_str(args, "url")?;
        Ok(PermissionRequest::new(
            ActionKind::Network,
            "browser.fetch",
            url,
            SideEffect::None,
            Trust::Model,
        ))
    }

    fn run(&self, args: &Value, _ctx: &ToolContext) -> Result<ToolOutput, VikramError> {
        let url = arg_str(args, "url")?;
        let max_length = args
            .get("max_length")
            .and_then(Value::as_u64)
            .map(|n| n as usize)
            .filter(|&n| n > 0)
            .unwrap_or(10_000);

        let client = match reqwest::blocking::Client::builder()
            .user_agent("Mozilla/5.0 (X11; Linux x86_64) AppleWebKit/537.36 (KHTML, like Gecko) Chrome/120.0.0.0 Safari/537.36")
            .timeout(Duration::from_secs(20))
            .build()
        {
            Ok(c) => c,
            Err(e) => {
                return Ok(ToolOutput {
                    content: format!("http client error: {e}"),
                    ok: false,
                });
            }
        };

        let response = match client.get(&url).send() {
            Ok(r) => r,
            Err(e) => {
                return Ok(ToolOutput {
                    content: format!("http request failed: {e}"),
                    ok: false,
                });
            }
        };

        if !response.status().is_success() {
            return Ok(ToolOutput {
                content: format!("http error: {}", response.status()),
                ok: false,
            });
        }

        let body = match response.text() {
            Ok(t) => t,
            Err(e) => {
                return Ok(ToolOutput {
                    content: format!("failed to read response body: {e}"),
                    ok: false,
                });
            }
        };

        let document = scraper::Html::parse_document(&body);
        let title = document
            .select(&scraper::Selector::parse("title").unwrap())
            .next()
            .map(|e| e.text().collect::<String>().trim().to_string())
            .unwrap_or_default();

        let text = if let Ok(sel) = scraper::Selector::parse("body") {
            document
                .select(&sel)
                .next()
                .map(|e| e.text().collect::<Vec<_>>().join(" "))
                .unwrap_or_else(|| body.clone())
        } else {
            body.clone()
        };

        // Collapse whitespace.
        let mut collapsed = String::with_capacity(text.len());
        let mut prev_space = true;
        for ch in text.chars() {
            if ch.is_whitespace() {
                if !prev_space {
                    collapsed.push(' ');
                }
                prev_space = true;
            } else {
                collapsed.push(ch);
                prev_space = false;
            }
        }
        let collapsed = collapsed.trim().to_string();

        let content = if collapsed.chars().count() > max_length {
            collapsed.chars().take(max_length).collect::<String>() + "... [truncated]"
        } else {
            collapsed
        };

        let out = format!("Title: {title}\nURL: {url}\n\n{content}");
        Ok(ToolOutput {
            content: truncated(out),
            ok: true,
        })
    }
}

// Alias for compatibility with alternate naming.
pub type BrowserFetchTool = BrowserFetch;

/// How long a DOM-aware browser call may take; `timeout_ms` overrides the module default.
fn browser_timeout(args: &Value) -> u64 {
    args.get("timeout_ms")
        .and_then(Value::as_u64)
        .unwrap_or(crate::browser::DEFAULT_TIMEOUT_MS)
        .clamp(1_000, 120_000)
}

/// The readable text of a page as the browser rendered it.
///
/// The difference from `browser.fetch` is the whole point: `--dump-dom` runs the page's scripts
/// first, so client-rendered content is visible. The readable text is a convenience; the serialized
/// DOM is kept behind `raw` so nothing is lost to the text projection.
pub struct BrowserDom;

impl Tool for BrowserDom {
    fn spec(&self) -> ToolSpec {
        ToolSpec {
            name: "browser.dom".into(),
            description: "Load a URL in headless Chromium and return the rendered page as text."
                .into(),
            schema: json!({
                "type": "object",
                "properties": {
                    "url": {"type": "string"},
                    "raw": {"type": "boolean"},
                    "timeout_ms": {"type": "integer"}
                },
                "required": ["url"]
            }),
        }
    }

    fn permission(&self, args: &Value) -> Result<PermissionRequest, VikramError> {
        let url = arg_str(args, "url")?;
        Ok(PermissionRequest::new(
            ActionKind::Network,
            "browser.dom",
            url,
            SideEffect::None,
            Trust::Model,
        ))
    }

    fn run(&self, args: &Value, _ctx: &ToolContext) -> Result<ToolOutput, VikramError> {
        let url = arg_str(args, "url")?;
        let raw = args.get("raw").and_then(Value::as_bool).unwrap_or(false);
        let dom = match crate::browser::navigate(&url, browser_timeout(args)) {
            Ok(dom) => dom,
            Err(e) => {
                return Ok(ToolOutput {
                    content: e,
                    ok: false,
                });
            }
        };
        if raw {
            return Ok(ToolOutput {
                content: truncated(dom),
                ok: true,
            });
        }
        let text = crate::browser::extract_text(&dom);
        let links = crate::browser::extract_links(&dom).len();
        let controls = crate::browser::extract_forms(&dom).len();
        let opening = format!(
            "URL: {url}\nbrowser: {}\nDOM: {} bytes, {links} links, {controls} form controls\n",
            crate::browser::describe_available(),
            dom.len()
        );
        let closing = "\n[this is the readable text only — the full serialized DOM is available by \
                       calling browser.dom again with raw=true]";
        if text.is_empty() {
            return Ok(ToolOutput {
                content: truncated(format!("{opening}\n(no readable text)\n{closing}")),
                ok: true,
            });
        }
        Ok(ToolOutput {
            content: truncated(format!("{opening}\n{text}\n{closing}")),
            ok: true,
        })
    }
}

/// Every link on a page, as the browser rendered it.
pub struct BrowserLinks;

impl Tool for BrowserLinks {
    fn spec(&self) -> ToolSpec {
        ToolSpec {
            name: "browser.links".into(),
            description: "Load a URL in headless Chromium and list its links (text and target)."
                .into(),
            schema: json!({
                "type": "object",
                "properties": {
                    "url": {"type": "string"},
                    "timeout_ms": {"type": "integer"}
                },
                "required": ["url"]
            }),
        }
    }

    fn permission(&self, args: &Value) -> Result<PermissionRequest, VikramError> {
        let url = arg_str(args, "url")?;
        Ok(PermissionRequest::new(
            ActionKind::Network,
            "browser.links",
            url,
            SideEffect::None,
            Trust::Model,
        ))
    }

    fn run(&self, args: &Value, _ctx: &ToolContext) -> Result<ToolOutput, VikramError> {
        let url = arg_str(args, "url")?;
        let dom = match crate::browser::navigate(&url, browser_timeout(args)) {
            Ok(dom) => dom,
            Err(e) => {
                return Ok(ToolOutput {
                    content: e,
                    ok: false,
                });
            }
        };
        let links = crate::browser::extract_links(&dom);
        if links.is_empty() {
            return Ok(ToolOutput {
                content: format!("no links on {url}"),
                ok: true,
            });
        }
        let mut body = format!("URL: {url}\n{} links\n", links.len());
        for (index, (href, text)) in links.iter().enumerate() {
            if text.is_empty() {
                body.push_str(&format!("{}. {href}\n", index + 1));
            } else {
                body.push_str(&format!("{}. {text}\n   {href}\n", index + 1));
            }
        }
        Ok(ToolOutput {
            content: truncated(body),
            ok: true,
        })
    }
}

/// A PNG of a page, written inside the workspace.
pub struct BrowserScreenshot;

impl Tool for BrowserScreenshot {
    fn spec(&self) -> ToolSpec {
        ToolSpec {
            name: "browser.screenshot".into(),
            description: "Render a URL to a PNG with headless Chromium, inside the workspace."
                .into(),
            schema: json!({
                "type": "object",
                "properties": {
                    "url": {"type": "string"},
                    "path": {"type": "string"},
                    "width": {"type": "integer"},
                    "height": {"type": "integer"},
                    "timeout_ms": {"type": "integer"}
                },
                "required": ["url", "path"]
            }),
        }
    }

    fn permission(&self, args: &Value) -> Result<PermissionRequest, VikramError> {
        let _url = arg_str(args, "url")?;
        let path = arg_str(args, "path")?;
        Ok(PermissionRequest::new(
            ActionKind::Network,
            "browser.screenshot",
            path,
            SideEffect::LocalMutation,
            Trust::Model,
        ))
    }

    fn run(&self, args: &Value, ctx: &ToolContext) -> Result<ToolOutput, VikramError> {
        let url = arg_str(args, "url")?;
        let raw_path = arg_str(args, "path")?;
        // The workspace check happens before the browser is even looked for: a refusal must not
        // depend on what is installed.
        let (path, display) = canonical_resource(&ctx.workspace_root, &raw_path)?;
        let width = args.get("width").and_then(Value::as_u64).unwrap_or(1280);
        let height = args.get("height").and_then(Value::as_u64).unwrap_or(800);
        let width = width.clamp(1, u64::from(crate::browser::MAX_DIMENSION)) as u32;
        let height = height.clamp(1, u64::from(crate::browser::MAX_DIMENSION)) as u32;
        match crate::browser::screenshot(&url, &path, width, height) {
            Ok(written) => Ok(ToolOutput {
                content: format!("wrote {width}x{height} screenshot of {url} to {written}"),
                ok: true,
            }),
            Err(e) => Ok(ToolOutput {
                content: format!("{e} (target was `{display}`)"),
                ok: false,
            }),
        }
    }
}

// ---------------------------------------------------------------------------------------------
// Interactive browser sessions
// ---------------------------------------------------------------------------------------------
//
// `browser.dom` renders a page and throws it away; these tools keep one page alive so the model can
// click a button, fill a field and read the result. Every one of them names its session, so an
// interaction can be refused when it belongs to a session policy has never seen. The resource is
// the session id rather than the selector: the session is the thing that was opened against a URL.

/// Open a live, interactive page.
pub struct BrowserOpen;

impl Tool for BrowserOpen {
    fn spec(&self) -> ToolSpec {
        ToolSpec {
            name: "browser.open".into(),
            description: "Open a URL in a headless Chromium session that stays alive for further \
                          interaction (browser.click, browser.type, browser.scroll, browser.text). \
                          Returns the session id the other browser.* tools take."
                .into(),
            schema: json!({
                "type": "object",
                "properties": {"url": {"type": "string"}},
                "required": ["url"]
            }),
        }
    }

    fn permission(&self, args: &Value) -> Result<PermissionRequest, VikramError> {
        let url = arg_str(args, "url")?;
        Ok(PermissionRequest::new(
            ActionKind::Network,
            "browser.open",
            url,
            SideEffect::None,
            Trust::Model,
        ))
    }

    fn run(&self, args: &Value, _ctx: &ToolContext) -> Result<ToolOutput, VikramError> {
        let url = arg_str(args, "url")?;
        match crate::browser::open(&url) {
            Ok(session) => Ok(ToolOutput {
                content: format!(
                    "session {session} opened on {url}\n{} session(s) live; close it with \
                     browser.close when the page is no longer needed",
                    crate::browser::count()
                ),
                ok: true,
            }),
            Err(e) => Ok(ToolOutput {
                content: e,
                ok: false,
            }),
        }
    }
}

/// Evaluate JavaScript in a live session.
pub struct BrowserEval;

impl Tool for BrowserEval {
    fn spec(&self) -> ToolSpec {
        ToolSpec {
            name: "browser.eval".into(),
            description:
                "Run JavaScript in an open browser session and return the result; objects \
                          come back as JSON."
                    .into(),
            schema: json!({
                "type": "object",
                "properties": {
                    "session": {"type": "string"},
                    "js": {"type": "string"}
                },
                "required": ["session", "js"]
            }),
        }
    }

    fn permission(&self, args: &Value) -> Result<PermissionRequest, VikramError> {
        let session = arg_str(args, "session")?;
        Ok(PermissionRequest::new(
            ActionKind::Execute,
            "browser.eval",
            session,
            SideEffect::None,
            Trust::Model,
        ))
    }

    fn run(&self, args: &Value, _ctx: &ToolContext) -> Result<ToolOutput, VikramError> {
        let session = arg_str(args, "session")?;
        let js = arg_str(args, "js")?;
        match crate::browser::eval(&session, &js) {
            Ok(value) => Ok(ToolOutput {
                content: truncated(value),
                ok: true,
            }),
            Err(e) => Ok(ToolOutput {
                content: e,
                ok: false,
            }),
        }
    }
}

/// Click an element by CSS selector.
pub struct BrowserClick;

impl Tool for BrowserClick {
    fn spec(&self) -> ToolSpec {
        ToolSpec {
            name: "browser.click".into(),
            description: "Click the first element matching a CSS selector in an open browser \
                          session."
                .into(),
            schema: json!({
                "type": "object",
                "properties": {
                    "session": {"type": "string"},
                    "selector": {"type": "string"}
                },
                "required": ["session", "selector"]
            }),
        }
    }

    fn permission(&self, args: &Value) -> Result<PermissionRequest, VikramError> {
        let session = arg_str(args, "session")?;
        Ok(PermissionRequest::new(
            ActionKind::Execute,
            "browser.click",
            session,
            // A click changes the page, and a page can submit a form: that is a local mutation.
            SideEffect::LocalMutation,
            Trust::Model,
        ))
    }

    fn run(&self, args: &Value, _ctx: &ToolContext) -> Result<ToolOutput, VikramError> {
        let session = arg_str(args, "session")?;
        let selector = arg_str(args, "selector")?;
        match crate::browser::click(&session, &selector) {
            Ok(report) => Ok(ToolOutput {
                content: report,
                ok: true,
            }),
            Err(e) => Ok(ToolOutput {
                content: e,
                ok: false,
            }),
        }
    }
}

/// Set the value of an input field.
pub struct BrowserType;

impl Tool for BrowserType {
    fn spec(&self) -> ToolSpec {
        ToolSpec {
            name: "browser.type".into(),
            description: "Focus the first element matching a CSS selector in an open browser \
                          session and set its value, dispatching input and change events."
                .into(),
            schema: json!({
                "type": "object",
                "properties": {
                    "session": {"type": "string"},
                    "selector": {"type": "string"},
                    "text": {"type": "string"}
                },
                "required": ["session", "selector", "text"]
            }),
        }
    }

    fn permission(&self, args: &Value) -> Result<PermissionRequest, VikramError> {
        let session = arg_str(args, "session")?;
        Ok(PermissionRequest::new(
            ActionKind::Execute,
            "browser.type",
            session,
            SideEffect::LocalMutation,
            Trust::Model,
        ))
    }

    fn run(&self, args: &Value, _ctx: &ToolContext) -> Result<ToolOutput, VikramError> {
        let session = arg_str(args, "session")?;
        let selector = arg_str(args, "selector")?;
        let text = arg_str(args, "text")?;
        match crate::browser::type_text(&session, &selector, &text) {
            Ok(report) => Ok(ToolOutput {
                content: report,
                ok: true,
            }),
            Err(e) => Ok(ToolOutput {
                content: e,
                ok: false,
            }),
        }
    }
}

/// Scroll the page.
pub struct BrowserScroll;

impl Tool for BrowserScroll {
    fn spec(&self) -> ToolSpec {
        ToolSpec {
            name: "browser.scroll".into(),
            description: "Scroll an open browser session's page by a number of pixels and report \
                          the new offset."
                .into(),
            schema: json!({
                "type": "object",
                "properties": {
                    "session": {"type": "string"},
                    "x": {"type": "integer"},
                    "y": {"type": "integer"}
                },
                "required": ["session"]
            }),
        }
    }

    fn permission(&self, args: &Value) -> Result<PermissionRequest, VikramError> {
        let session = arg_str(args, "session")?;
        Ok(PermissionRequest::new(
            ActionKind::Execute,
            "browser.scroll",
            session,
            SideEffect::None,
            Trust::Model,
        ))
    }

    fn run(&self, args: &Value, _ctx: &ToolContext) -> Result<ToolOutput, VikramError> {
        let session = arg_str(args, "session")?;
        let x = args.get("x").and_then(Value::as_i64).unwrap_or(0);
        let y = args.get("y").and_then(Value::as_i64).unwrap_or(0);
        if x == 0 && y == 0 {
            return Ok(ToolOutput {
                content: "nothing to do: both `x` and `y` are zero".to_string(),
                ok: false,
            });
        }
        match crate::browser::scroll(&session, x, y) {
            Ok(report) => Ok(ToolOutput {
                content: report,
                ok: true,
            }),
            Err(e) => Ok(ToolOutput {
                content: e,
                ok: false,
            }),
        }
    }
}

/// The visible text of a live page.
pub struct BrowserText;

impl Tool for BrowserText {
    fn spec(&self) -> ToolSpec {
        ToolSpec {
            name: "browser.text".into(),
            description:
                "Return the visible text of an open browser session's current page, after \
                          any interaction."
                    .into(),
            schema: json!({
                "type": "object",
                "properties": {"session": {"type": "string"}},
                "required": ["session"]
            }),
        }
    }

    fn permission(&self, args: &Value) -> Result<PermissionRequest, VikramError> {
        let session = arg_str(args, "session")?;
        Ok(PermissionRequest::new(
            ActionKind::Read,
            "browser.text",
            session,
            SideEffect::None,
            Trust::Model,
        ))
    }

    fn run(&self, args: &Value, _ctx: &ToolContext) -> Result<ToolOutput, VikramError> {
        let session = arg_str(args, "session")?;
        match crate::browser::text(&session) {
            Ok(text) if text.trim().is_empty() => Ok(ToolOutput {
                content: "(the page has no visible text)".to_string(),
                ok: true,
            }),
            Ok(text) => Ok(ToolOutput {
                content: truncated(text),
                ok: true,
            }),
            Err(e) => Ok(ToolOutput {
                content: e,
                ok: false,
            }),
        }
    }
}

/// End a session.
pub struct BrowserClose;

impl Tool for BrowserClose {
    fn spec(&self) -> ToolSpec {
        ToolSpec {
            name: "browser.close".into(),
            description: "Close an open browser session and stop its browser process.".into(),
            schema: json!({
                "type": "object",
                "properties": {"session": {"type": "string"}},
                "required": ["session"]
            }),
        }
    }

    fn permission(&self, args: &Value) -> Result<PermissionRequest, VikramError> {
        let session = arg_str(args, "session")?;
        Ok(PermissionRequest::new(
            ActionKind::Execute,
            "browser.close",
            session,
            // The process being stopped is one this process started; nothing outside is touched.
            SideEffect::None,
            Trust::Model,
        ))
    }

    fn run(&self, args: &Value, _ctx: &ToolContext) -> Result<ToolOutput, VikramError> {
        let session = arg_str(args, "session")?;
        match crate::browser::close(&session) {
            Ok(()) => Ok(ToolOutput {
                content: format!(
                    "closed {session}; {} session(s) live",
                    crate::browser::count()
                ),
                ok: true,
            }),
            Err(e) => Ok(ToolOutput {
                content: e,
                ok: false,
            }),
        }
    }
}

/// Which sessions are live.
pub struct BrowserSessions;

impl Tool for BrowserSessions {
    fn spec(&self) -> ToolSpec {
        ToolSpec {
            name: "browser.sessions".into(),
            description: "List the live browser sessions of this process and the page each is on."
                .into(),
            schema: json!({"type": "object", "properties": {}}),
        }
    }

    fn permission(&self, _args: &Value) -> Result<PermissionRequest, VikramError> {
        Ok(PermissionRequest::new(
            ActionKind::Read,
            "browser.sessions",
            "*",
            SideEffect::None,
            Trust::Model,
        ))
    }

    fn run(&self, _args: &Value, _ctx: &ToolContext) -> Result<ToolOutput, VikramError> {
        let sessions = crate::browser::describe_sessions();
        if sessions.is_empty() {
            return Ok(ToolOutput {
                content: format!(
                    "no browser sessions are open (browser: {})",
                    crate::browser::describe_available()
                ),
                ok: true,
            });
        }
        let mut body = format!("{} session(s):\n", sessions.len());
        for line in sessions {
            body.push_str(&format!("- {line}\n"));
        }
        Ok(ToolOutput {
            content: truncated(body),
            ok: true,
        })
    }
}

pub struct CodeReview;

impl Tool for CodeReview {
    fn spec(&self) -> ToolSpec {
        ToolSpec {
            name: "code.review".into(),
            description: "Collect code changes for review.".into(),
            schema: json!({
                "type": "object",
                "properties": {
                    "base": {"type": "string"},
                    "scope": {"type": "string", "enum": ["staged", "unstaged", "all"]}
                }
            }),
        }
    }

    fn permission(&self, args: &Value) -> Result<PermissionRequest, VikramError> {
        let base = args.get("base").and_then(Value::as_str).unwrap_or("HEAD");
        Ok(PermissionRequest::new(
            ActionKind::Read,
            "code.review",
            base,
            SideEffect::None,
            Trust::Model,
        ))
    }

    fn run(&self, args: &Value, ctx: &ToolContext) -> Result<ToolOutput, VikramError> {
        let base = args
            .get("base")
            .and_then(Value::as_str)
            .unwrap_or("HEAD")
            .to_string();
        let scope = args
            .get("scope")
            .and_then(Value::as_str)
            .unwrap_or("all")
            .to_string();

        let mut base_args: Vec<String> = vec!["diff".to_string()];
        match scope.as_str() {
            "staged" => {
                base_args.push("--cached".to_string());
                base_args.push(base.clone());
            }
            "unstaged" => {}
            _ => {
                base_args.push(base.clone());
            }
        }

        let mut stat_args = base_args.clone();
        stat_args.push("--stat".to_string());

        let run_git = |gargs: &[String]| -> Result<std::process::Output, std::io::Error> {
            std::process::Command::new("git")
                .args(gargs)
                .current_dir(&ctx.workspace_root)
                .output()
        };

        let stat_out = match run_git(&stat_args) {
            Ok(o) => o,
            Err(e) => {
                return Ok(ToolOutput {
                    content: format!("failed to run git: {e}"),
                    ok: false,
                });
            }
        };
        let diff_out = match run_git(&base_args) {
            Ok(o) => o,
            Err(e) => {
                return Ok(ToolOutput {
                    content: format!("failed to run git: {e}"),
                    ok: false,
                });
            }
        };

        let stat = String::from_utf8_lossy(&stat_out.stdout).trim().to_string();
        let diff = String::from_utf8_lossy(&diff_out.stdout).trim().to_string();

        let ok = stat_out.status.success() && diff_out.status.success();

        let content = if !ok {
            let mut err = String::from_utf8_lossy(&diff_out.stderr).to_string();
            if err.trim().is_empty() {
                err = String::from_utf8_lossy(&stat_out.stderr).to_string();
            }
            format!("git diff failed: {}", err.trim())
        } else {
            format!(
                "=== Diff Stat ===\n{}\n\n=== Diff ===\n{}\n\nReview the above changes for bugs, security issues, and performance concerns.",
                stat, diff
            )
        };

        Ok(ToolOutput {
            content: truncated(content),
            ok,
        })
    }
}

// Alias for compatibility with alternate naming.
pub type CodeReviewTool = CodeReview;

// ---------------------------------------------------------------------------------------------
// Language server tools
//
// These are read-only queries against a real language server, so they inherit the read permission
// shape and declare no side effect. The server process itself is started and kept warm by
// [`LspPool`]; a missing server is reported as a failed observation with the install hint, never as
// a silent empty answer.
// ---------------------------------------------------------------------------------------------
