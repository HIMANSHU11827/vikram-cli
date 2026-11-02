//! What a tool row shows once it is opened: the receipt for one call.
//!
//! A row is one line while it is shut and a labelled block while it is open, built only from the
//! payload the runtime reported for that call. Nothing here invents content: when the runtime sent
//! nothing, the row says so in those words.

use ratatui::style::Style;
use ratatui::text::{Line, Span};
use serde_json::Value;
use unicode_width::UnicodeWidthStr;

use crate::app::{Payload, SessionView, ThoughtKey};
use crate::theme::Theme;

/// A click target: the row it sits on, the last column of its label, the fold it toggles, and
/// whether that fold lives in the tool key space.
pub type Mark = (usize, u16, ThoughtKey, bool);

/// Nested sections fold in their own key space, so opening one never opens the row it lives in.
pub const ACTIVITY_SECTION: usize = usize::MAX - 2;

/// Missing text, in the words the transcript uses for it wherever a body is gone.
pub const MISSING: &str = "Deleted content unavailable";

/// How many lines of a long body survive before the row counts the rest instead of printing it.
const BODY_CAP: usize = 20;

/// How many search results a single row will list.
const RESULT_CAP: usize = 8;

/// What every line inside a row is indented by, so the row's own line stays the only header.
const BODY_INDENT: &str = "     ";

/// One row on screen, with everything the body needs to draw itself.
pub struct Row<'a> {
    pub payload: &'a Payload,
    pub key: ThoughtKey,
    /// Where this row sits in the activity list; nested folds derive their keys from it.
    pub position: usize,
    pub open: bool,
    /// `None` while the call is still out.
    pub ok: Option<bool>,
    pub waiting: bool,
    pub width: usize,
    pub session: &'a SessionView,
    pub theme: &'a Theme,
}

/// What kind of work the call was, which decides both its label and its body.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Kind {
    Wrote,
    Edited,
    Read,
    Deleted,
    Ran,
    Terminal,
    Search,
    Delegated,
    Skill,
    Mcp,
    Plugin,
    Other,
}

pub fn kind_of(name: &str) -> Kind {
    match name {
        "fs.write" => Kind::Wrote,
        "fs.edit" => Kind::Edited,
        "fs.read" | "fs.list" => Kind::Read,
        "fs.delete" => Kind::Deleted,
        "shell.run" | "test.run" | "build.run" | "git" => Kind::Ran,
        "terminal" => Kind::Terminal,
        "web.search" | "web_search" | "search.text" => Kind::Search,
        "team.delegate" | "hive.delegate" => Kind::Delegated,
        name if name.starts_with("skill") => Kind::Skill,
        name if name.starts_with("plugin") => Kind::Plugin,
        name if name.starts_with("mcp") => Kind::Mcp,
        _ => Kind::Other,
    }
}

/// Draw one row, shut or open.
pub fn push_row(lines: &mut Vec<Line<'static>>, marks: &mut Vec<Mark>, row: Row<'_>) {
    let theme = row.theme;
    let args = arguments(row.payload);
    let kind = kind_of(&row.payload.name);
    let (label, detail) = label_for(kind, row.payload, &args, theme);
    let glyph = glyph_for(row.ok, row.waiting, theme);
    let arrow = pointing(theme, row.open);

    let head = format!("{glyph} {label}");
    draw_head(lines, marks, &row, &head, detail.as_deref(), arrow, glyph);

    if !row.open {
        return;
    }
    let mut ctx = Ctx {
        lines,
        marks,
        theme,
        session: row.session,
        position: row.position,
        width: row.width,
    };
    match kind {
        Kind::Wrote => ctx.file(&args, true, row.payload),
        Kind::Read => ctx.numbered(&row.payload.content),
        Kind::Edited => ctx.diff(&args, row.payload),
        Kind::Deleted => ctx.deleted(&row.payload.content),
        Kind::Ran | Kind::Terminal => ctx.shell(&args, &row),
        Kind::Search => ctx.search(&row.payload.content),
        Kind::Delegated => ctx.delegated(&args, &row.payload.content),
        Kind::Skill => ctx.skill(&args, &row.payload.content),
        Kind::Mcp => ctx.mcp(&args, &row.payload.content),
        Kind::Plugin => ctx.plugin(&args, &row.payload.content),
        Kind::Other => {
            ctx.field("Tool", &row.payload.name);
            if !row.payload.resource.is_empty() {
                ctx.field("Target", &row.payload.resource);
            }
            ctx.tail(&row.payload.content, ctx.theme.body());
        }
    }
}

fn draw_head(
    lines: &mut Vec<Line<'static>>,
    marks: &mut Vec<Mark>,
    row: &Row<'_>,
    head: &str,
    detail: Option<&str>,
    arrow: &'static str,
    glyph: &'static str,
) {
    let theme = row.theme;
    let style = match row.ok {
        Some(true) => Style::default().fg(theme.tool),
        Some(false) => Style::default().fg(theme.error),
        None if row.waiting => Style::default().fg(theme.accent),
        None => theme.secondary(),
    };
    let mut spans = vec![
        Span::styled(format!("{glyph} "), style),
        Span::styled(
            head.trim_start_matches(glyph).trim_start().to_string(),
            Style::default().fg(theme.text),
        ),
    ];
    let mut used = UnicodeWidthStr::width(head);
    if let Some(detail) = detail {
        if !detail.is_empty() {
            spans.push(Span::raw("  "));
            spans.push(Span::styled(
                truncate(&redact(detail), row.width.saturating_sub(used).saturating_sub(4)),
                theme.muted(),
            ));
            used += 2 + UnicodeWidthStr::width(detail);
        }
    }
    // The arrow sits right after the label instead of being pushed to the right edge: a right-aligned
    // arrow has to be measured against the drawable width, and one column of error clips it.
    spans.push(Span::raw("  "));
    spans.push(Span::styled(arrow, theme.muted()));

    let mark_width = (used + 2 + UnicodeWidthStr::width(arrow)) as u16;
    marks.push((lines.len(), mark_width, row.key, true));
    lines.push(Line::from(spans));
}

fn glyph_for(ok: Option<bool>, waiting: bool, theme: &Theme) -> &'static str {
    match (ok, waiting) {
        (Some(true), _) => theme.check(),
        (Some(false), _) => theme.cross(),
        (None, true) => {
            if theme.ascii {
                "!"
            } else {
                "⏸"
            }
        }
        (None, false) => {
            if theme.ascii {
                "->"
            } else {
                "→"
            }
        }
    }
}

fn pointing(theme: &Theme, open: bool) -> &'static str {
    match (theme.ascii, open) {
        (true, true) => "v",
        (true, false) => ">",
        (false, true) => "▼",
        (false, false) => "▶",
    }
}

fn label_for(kind: Kind, payload: &Payload, args: &Value, theme: &Theme) -> (String, Option<String>) {
    let path = || path_of(args, payload);
    let pair = |name: &str, detail: String| (name.to_string(), Some(detail));
    match kind {
        Kind::Wrote => pair("Wrote", path()),
        Kind::Read => pair("Read", path()),
        Kind::Deleted => pair("Deleted", path()),
        Kind::Edited => {
            let counts = edit_counts(args)
                .map(|(added, removed)| format!("  (+{added} -{removed})"))
                .unwrap_or_default();
            pair("Edited", format!("{}{counts}", path()))
        }
        Kind::Ran => pair(
            "Ran",
            string(args, &["command", "cmd"]).unwrap_or_else(|| payload.resource.clone()),
        ),
        Kind::Terminal => pair("Terminal", terminal_detail(args, payload)),
        Kind::Search => {
            let query = string(args, &["query", "q"]).unwrap_or_else(|| payload.resource.clone());
            pair("Web Search", format!("\"{}\"", truncate(&query, 60)))
        }
        Kind::Delegated => {
            let task = string(args, &["task", "prompt", "goal"]).unwrap_or_else(|| "delegate".into());
            let agent = string(args, &["agent", "to", "assignee"]);
            let detail = match agent {
                Some(agent) => format!("{} to {}", truncate(&task, 44), agent),
                None => truncate(&task, 52),
            };
            pair("Delegated", detail)
        }
        Kind::Skill => pair(
            "Skill",
            string(args, &["name", "skill", "id"]).unwrap_or_else(|| "skill".into()),
        ),
        Kind::Mcp => {
            let server = string(args, &["server", "mcp"]).unwrap_or_else(|| "mcp".into());
            let action = string(args, &["action", "tool", "method"]).unwrap_or_default();
            let detail = if action.is_empty() {
                server
            } else {
                format!("{server} — {action}")
            };
            pair("MCP", detail)
        }
        Kind::Plugin => {
            let plugin = string(args, &["plugin", "name"]).unwrap_or_else(|| "plugin".into());
            let action = string(args, &["action", "command"]).unwrap_or_default();
            let detail = if action.is_empty() {
                plugin
            } else {
                format!("{plugin} — {action}")
            };
            pair("Plugin", detail)
        }
        Kind::Other => {
            let _ = theme;
            (humanize(&payload.name), None)
        }
    }
}

fn terminal_detail(args: &Value, payload: &Payload) -> String {
    let command = string(args, &["command"]);
    if payload.cwd.is_empty() {
        command.unwrap_or_else(|| payload.resource.clone())
    } else if let Some(command) = command {
        format!("{command} · {}", payload.cwd)
    } else {
        payload.cwd.clone()
    }
}

/// How a body is drawn, one row's worth of state at a time.
struct Ctx<'a> {
    lines: &'a mut Vec<Line<'static>>,
    marks: &'a mut Vec<Mark>,
    theme: &'a Theme,
    session: &'a SessionView,
    position: usize,
    width: usize,
}

impl Ctx<'_> {
    fn line(&mut self, text: impl Into<String>, style: Style) {
        let text = text.into();
        self.lines
            .push(Line::from(Span::styled(format!("{BODY_INDENT}{text}"), style)));
    }

    /// A labelled field: the name in the muted style, the value in the body style.
    fn field(&mut self, name: &str, value: &str) {
        if value.trim().is_empty() {
            return;
        }
        let value = clip(&redact(value), self.width.saturating_sub(BODY_INDENT.len() + name.len() + 2));
        let theme = self.theme;
        self.lines.push(Line::from(vec![
            Span::styled(format!("{BODY_INDENT}{name}  "), theme.muted()),
            Span::styled(value, theme.body()),
        ]));
    }

    /// The tail of a body: printed whole when it is short, counted when it is not.
    fn tail(&mut self, text: &str, style: Style) {
        let text = redact(text);
        let lines: Vec<&str> = text.lines().collect();
        for line in lines.iter().take(BODY_CAP) {
            self.line(clip(line, self.width.saturating_sub(BODY_INDENT.len())), style);
        }
        if lines.len() > BODY_CAP {
            let more = lines.len() - BODY_CAP;
            self.line(format!("({more} more lines)"), self.theme.muted());
        }
    }

    /// File content with a line-number gutter, so a body reads as the file it came from.
    fn numbered(&mut self, text: &str) {
        let text = redact(text);
        if text.trim().is_empty() {
            self.line(MISSING.to_string(), self.theme.muted());
            return;
        }
        let all: Vec<&str> = text.lines().collect();
        let numbers = all.len().to_string().len();
        let style = self.theme.body();
        for (offset, line) in all.iter().take(BODY_CAP).enumerate() {
            self.line(
                format!(
                    "{:>numbers$}  {}",
                    offset + 1,
                    clip(line, self.width.saturating_sub(numbers + BODY_INDENT.len() + 2)),
                    numbers = numbers
                ),
                style,
            );
        }
        if all.len() > BODY_CAP {
            let more = all.len() - BODY_CAP;
            self.line(format!("({more} more lines)"), self.theme.muted());
        }
    }

    fn file(&mut self, args: &Value, numbered: bool, payload: &Payload) {
        self.field("File", &path_of(args, payload));
        match string(args, &["content", "text"]) {
            Some(content) if numbered => self.numbered(&content),
            _ => self.tail(&payload.content, self.theme.body()),
        }
    }

    fn diff(&mut self, args: &Value, payload: &Payload) {
        self.field("File", &path_of(args, payload));
        let old = string(args, &["old_string", "old", "before"]);
        let new = string(args, &["new_string", "new", "after"]);
        match (old, new) {
            (Some(old), Some(new)) => {
                let mut emitted = 0;
                for line in old.lines() {
                    self.line(format!("- {line}"), Style::default().fg(self.theme.error));
                    emitted += 1;
                }
                for line in new.lines() {
                    self.line(format!("+ {line}"), Style::default().fg(self.theme.tool));
                    emitted += 1;
                }
                if emitted == 0 {
                    self.line(MISSING.to_string(), self.theme.muted());
                }
            }
            // A write reports what it wrote; an edit whose arguments are gone still has a summary.
            _ => self.tail(&payload.content, self.theme.body()),
        }
    }

    fn deleted(&mut self, content: &str) {
        let marker = "--- deleted content ---";
        let body = content
            .split_once(marker)
            .map(|(_, body)| body)
            .unwrap_or("");
        if body.trim().is_empty() {
            self.line(MISSING.to_string(), self.theme.muted());
            return;
        }
        self.numbered(body);
    }

    fn shell(&mut self, args: &Value, row: &Row<'_>) {
        let payload = row.payload;
        if let Some(command) = string(args, &["command", "cmd"]) {
            self.field("Command", &command);
        }
        self.field("Directory", &payload.cwd);
        let shell = parse_shell(&payload.content);
        if let Some(exit) = shell.exit {
            let verdict = if exit == 0 { "Success" } else { "Failed" };
            self.field("Exit code", &format!("{exit} · {verdict}"));
        }
        if payload.duration_ms > 0 {
            self.field("Duration", &format!("{:.1}s", payload.duration_ms as f64 / 1000.0));
        }
        if !shell.stdout.trim().is_empty() {
            self.section(1, "Stdout", &shell.stdout);
        }
        if !shell.stderr.trim().is_empty() {
            // A failure is the reason the row was opened, so its stderr does not wait for a second
            // click: it prints where the eye already is. Successes still fold it away.
            if row.ok == Some(false) {
                self.line("Stderr", Style::default().fg(self.theme.error));
                self.tail(&shell.stderr, Style::default().fg(self.theme.error));
            } else {
                self.section(2, "Stderr", &shell.stderr);
            }
        }
        if shell.stdout.trim().is_empty() && shell.stderr.trim().is_empty() {
            self.line("(no output)", self.theme.muted());
        }
    }

    fn search(&mut self, content: &str) {
        let results = parse_search(content);
        if results.is_empty() {
            self.line("0 results", self.theme.muted());
            self.tail(content, self.theme.muted());
            return;
        }
        self.line(format!("{} results", results.len()), self.theme.muted());
        for (index, result) in results.iter().take(RESULT_CAP).enumerate() {
            self.line(
                format!("{}. {}", index + 1, clip(&result.title, self.width.saturating_sub(6))),
                Style::default().fg(self.theme.text),
            );
            if !result.url.is_empty() {
                self.line(
                    clip(&result.url, self.width.saturating_sub(BODY_INDENT.len())),
                    Style::default().fg(self.theme.accent),
                );
            }
            if !result.snippet.is_empty() {
                let body = format!("{}\n{}", result.title, result.snippet);
                self.section(2 + index, "Details", &body);
            }
        }
    }

    fn delegated(&mut self, args: &Value, content: &str) {
        let task = string(args, &["task", "prompt", "goal"]).unwrap_or_default();
        let agent = string(args, &["agent", "to", "assignee"]).unwrap_or_default();
        self.field("Agent", if agent.is_empty() { "unassigned" } else { &agent });
        self.field("Task", &task);
        self.field("Assigned by", "VIKRAM");
        let status = if content.trim().is_empty() { "Working" } else { "Reported" };
        self.field("Status", status);
        if let Some(instructions) = string(args, &["instructions", "context", "shared_context"]) {
            self.section(1, "Instructions", &instructions);
        }
        if !content.trim().is_empty() {
            self.section(2, "Response", content);
        }
    }

    fn skill(&mut self, args: &Value, content: &str) {
        let name = string(args, &["name", "skill", "id"]).unwrap_or_default();
        self.field("Name", &name);
        if let Some(description) = content.lines().next() {
            self.field("Description", description);
        }
        self.field("State", "loaded");
        self.tail(content, self.theme.body());
    }

    fn mcp(&mut self, args: &Value, content: &str) {
        self.field("Server", &string(args, &["server", "mcp"]).unwrap_or_else(|| "mcp".into()));
        self.field("Action", &string(args, &["action", "tool", "method"]).unwrap_or_default());
        self.field("Target", &string(args, &["target", "path", "resource"]).unwrap_or_default());
        if let Some(status) = content.lines().next() {
            self.field("Status", status);
        }
        self.tail(content, self.theme.body());
    }

    fn plugin(&mut self, args: &Value, content: &str) {
        self.field("Plugin", &string(args, &["plugin", "name"]).unwrap_or_else(|| "plugin".into()));
        self.field("Action", &string(args, &["action", "command"]).unwrap_or_default());
        self.field(
            "Repository",
            &string(args, &["repository", "repo", "target"]).unwrap_or_default(),
        );
        if let Some(result) = content.lines().find(|line| !line.trim().is_empty()) {
            self.field("Result", result);
        }
        self.tail(content, self.theme.body());
    }

    /// A nested fold: its own line, its own click, and its body only when it is open.
    fn section(&mut self, index: usize, title: &str, body: &str) {
        let key: ThoughtKey = (ACTIVITY_SECTION, self.position * 8 + index);
        let open = self.session.tool_expanded(key);
        let arrow = pointing(self.theme, open);
        let head = format!("{title} {arrow}");
        self.marks.push((
            self.lines.len(),
            (BODY_INDENT.len() + head.width()) as u16,
            key,
            true,
        ));
        self.lines.push(Line::from(vec![
            Span::styled(format!("{BODY_INDENT}{title} "), self.theme.muted()),
            Span::styled(arrow, self.theme.muted()),
        ]));
        if open {
            self.tail(body, self.theme.body());
        }
    }
}

/// One search result, recovered from the renderer's text.
struct Result {
    title: String,
    url: String,
    snippet: String,
}

fn parse_search(content: &str) -> Vec<Result> {
    let mut results: Vec<Result> = Vec::new();
    for line in content.lines() {
        let trimmed = line.trim();
        if trimmed.is_empty() {
            continue;
        }
        let numbered = trimmed
            .split_once(". ")
            .filter(|(head, _)| head.chars().all(|c| c.is_ascii_digit()))
            .map(|(_, rest)| rest.to_string());
        if let Some(title) = numbered {
            results.push(Result {
                title,
                url: String::new(),
                snippet: String::new(),
            });
            continue;
        }
        let Some(current) = results.last_mut() else {
            continue;
        };
        if let Some(url) = trimmed.strip_prefix("URL:") {
            current.url = url.trim().to_string();
        } else if current.snippet.is_empty() {
            current.snippet = trimmed.to_string();
        } else {
            current.snippet.push(' ');
            current.snippet.push_str(trimmed);
        }
    }
    results
}

struct Shell {
    stdout: String,
    stderr: String,
    exit: Option<i32>,
}

/// The shell tool folds its exit code and its stderr into the same body, so the row has to take
/// them apart again to label them.
fn parse_shell(content: &str) -> Shell {
    let mut body = content;
    let mut exit = None;
    if let Some((head, tail)) = content.rsplit_once("[exit ") {
        if let Some(code) = tail.trim().trim_end_matches(']').parse::<i32>().ok() {
            exit = Some(code);
        }
        body = head;
    }
    let (stdout, stderr) = match body.split_once("[stderr]\n") {
        Some((out, err)) => (out, err),
        None => (body, ""),
    };
    Shell {
        stdout: stdout.trim_end().to_string(),
        stderr: stderr.trim_end().to_string(),
        exit,
    }
}

/// How many lines an edit added and removed: the number on the row has to match the diff under it.
fn edit_counts(args: &Value) -> Option<(usize, usize)> {
    let old = string(args, &["old_string", "old", "before"])?;
    let new = string(args, &["new_string", "new", "after"])?;
    Some((new.lines().count(), old.lines().count()))
}

fn arguments(payload: &Payload) -> Value {
    serde_json::from_str(&payload.arguments).unwrap_or(Value::Null)
}

fn string(args: &Value, keys: &[&str]) -> Option<String> {
    let object = args.as_object()?;
    for key in keys {
        if let Some(value) = object.get(*key).and_then(Value::as_str) {
            if !value.trim().is_empty() {
                return Some(value.to_string());
            }
        }
    }
    None
}

fn path_of(args: &Value, payload: &Payload) -> String {
    string(args, &["path", "file", "filename", "target"])
        .or_else(|| {
            if payload.resource.is_empty() {
                None
            } else {
                Some(payload.resource.clone())
            }
        })
        .unwrap_or_else(|| "(unknown file)".into())
}

fn humanize(name: &str) -> String {
    let spaced = name.replace('.', " ").replace('_', " ");
    let mut chars = spaced.chars();
    match chars.next() {
        Some(first) => first.to_uppercase().collect::<String>() + chars.as_str(),
        None => String::new(),
    }
}

/// Cut a line to the width the row can draw without wrapping into the next screen line.
fn clip(text: &str, width: usize) -> String {
    let width = width.max(4);
    if UnicodeWidthStr::width(text) <= width {
        return text.to_string();
    }
    let mut out = String::new();
    let mut used = 0;
    for ch in text.chars() {
        let w = UnicodeWidthStr::width(ch.to_string().as_str());
        if used + w + 1 > width {
            break;
        }
        out.push(ch);
        used += w;
    }
    out.push('…');
    out
}

fn truncate(text: &str, width: usize) -> String {
    clip(text, width.max(4))
}

/// Replace anything that looks like a credential before it can reach the screen. The renderer
/// redacts, not the runtime: the runtime keeps what it needs, and the screen keeps nothing.
pub fn redact(text: &str) -> String {
    // Split into alternating word and whitespace runs, so a header can be judged together with the
    // word that carries its secret. `Authorization: Bearer abc` names the secret in one word and
    // puts it in the next, and redacting only the name would leave the value in the open.
    let mut words: Vec<&str> = Vec::new();
    let mut gaps: Vec<&str> = Vec::new();
    let mut rest = text;
    while !rest.is_empty() {
        let split = rest.find(char::is_whitespace).unwrap_or(rest.len());
        words.push(&rest[..split]);
        rest = &rest[split..];
        let split = rest
            .find(|c: char| !c.is_whitespace())
            .unwrap_or(rest.len());
        gaps.push(&rest[..split]);
        rest = &rest[split..];
    }

    let mut out = String::with_capacity(text.len());
    // How many of the words that follow are the value of a name already written out.
    let mut swallow = 0usize;
    for (index, word) in words.iter().enumerate() {
        if swallow > 0 {
            // The marker lands where the value ends, so `Authorization: Bearer abc` becomes
            // `Authorization: [redacted]` rather than leaving a space where the value was.
            let last = swallow == 1 || index + 1 == words.len();
            if last {
                out.push_str("[redacted]");
                if let Some(gap) = gaps.get(index) {
                    out.push_str(gap);
                }
            }
            swallow -= 1;
            continue;
        }
        let rendered = redact_token(word);
        if rendered == *word && names_only(word) {
            swallow = 2;
        }
        out.push_str(&rendered);
        if let Some(gap) = gaps.get(index) {
            out.push_str(gap);
        }
    }
    out
}

/// A word that names a secret without carrying it: `Authorization:`, `--token`, `key=`.
fn names_only(word: &str) -> bool {
    let trimmed = word.trim_end_matches([':', '=']);
    trimmed.len() < word.len() && sensitive(trimmed)
}

fn redact_token(token: &str) -> String {
    if token.is_empty() {
        return String::new();
    }
    // `key=value` and `key: value` both carry secrets under a name that says so.
    for separator in ['=', ':'] {
        if let Some((key, value)) = token.split_once(separator) {
            if sensitive(key) && !value.is_empty() {
                return format!("{key}{separator}[redacted]");
            }
            if is_secret(value) {
                return format!("{key}{separator}[redacted]");
            }
        }
    }
    if is_secret(token) {
        return "[redacted]".into();
    }
    token.to_string()
}

fn sensitive(key: &str) -> bool {
    let key = key
        .trim_matches(|c: char| !c.is_alphanumeric() && c != '_' && c != '-')
        .to_ascii_lowercase();
    matches!(
        key.as_str(),
        "key"
            | "apikey"
            | "api_key"
            | "token"
            | "access_token"
            | "refresh_token"
            | "secret"
            | "client_secret"
            | "password"
            | "passwd"
            | "authorization"
            | "bearer"
            | "private_key"
            | "signing_key"
    )
}

fn is_secret(token: &str) -> bool {
    let bare = token.trim_matches(|c: char| {
        !c.is_alphanumeric() && c != '_' && c != '-' && c != '.' && c != '/' && c != '+'
    });
    const PREFIXES: [&str; 11] = [
        "sk-",
        "sk_",
        "ghp_",
        "gho_",
        "github_pat_",
        "glpat-",
        "xoxb-",
        "xoxp-",
        "AKIA",
        "AIza",
        "eyJ",
    ];
    if PREFIXES.iter().any(|prefix| bare.starts_with(prefix)) {
        return true;
    }
    // A JWT is three base64url segments and no other shape is that long with two dots in it.
    let parts: Vec<&str> = bare.split('.').collect();
    parts.len() == 3
        && bare.len() >= 40
        && parts
            .iter()
            .all(|part| part.chars().all(|c| c.is_alphanumeric() || c == '-' || c == '_'))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_keyed_secret_loses_its_value_but_keeps_its_name() {
        assert_eq!(redact("export API_KEY=abc123"), "export API_KEY=[redacted]");
        assert_eq!(redact("Authorization: Bearer abc"), "Authorization: [redacted]");
    }

    #[test]
    fn a_bare_token_is_replaced_whole() {
        assert_eq!(redact("token sk-live-1234"), "token [redacted]");
        assert_eq!(redact("see ghp_abcdefghij"), "see [redacted]");
    }

    #[test]
    fn ordinary_words_survive_redaction() {
        assert_eq!(redact("cargo test --all"), "cargo test --all");
        assert_eq!(redact("src/api/jwt.rs"), "src/api/jwt.rs");
    }

    #[test]
    fn a_shell_body_tells_its_exit_code_apart() {
        let shell = parse_shell("done\n[stderr]\nwarn: x\n[exit 0]");
        assert_eq!(shell.exit, Some(0));
        assert_eq!(shell.stdout, "done");
        assert_eq!(shell.stderr, "warn: x");
    }

    #[test]
    fn a_shell_failure_keeps_its_code() {
        assert_eq!(parse_shell("boom\n[exit 101]").exit, Some(101));
    }

    #[test]
    fn search_results_come_back_in_order_with_their_urls() {
        let body = "1. First\n   a snippet\n   URL: https://a.example\n2. Second\n   URL: https://b.example\n";
        let results = parse_search(body);
        assert_eq!(results.len(), 2);
        assert_eq!(results[0].title, "First");
        assert_eq!(results[0].url, "https://a.example");
        assert_eq!(results[1].title, "Second");
    }

    #[test]
    fn an_edit_counts_the_lines_it_touched() {
        let args = serde_json::json!({"old_string": "a", "new_string": "a\nb"});
        assert_eq!(edit_counts(&args), Some((2, 1)));
    }

    #[test]
    fn every_row_kind_is_recognised_by_its_tool() {
        assert_eq!(kind_of("fs.write"), Kind::Wrote);
        assert_eq!(kind_of("fs.edit"), Kind::Edited);
        assert_eq!(kind_of("fs.read"), Kind::Read);
        assert_eq!(kind_of("fs.delete"), Kind::Deleted);
        assert_eq!(kind_of("shell.run"), Kind::Ran);
        assert_eq!(kind_of("terminal"), Kind::Terminal);
        assert_eq!(kind_of("web.search"), Kind::Search);
        assert_eq!(kind_of("team.delegate"), Kind::Delegated);
        assert_eq!(kind_of("skill.load"), Kind::Skill);
        assert_eq!(kind_of("mcp.read"), Kind::Mcp);
        assert_eq!(kind_of("plugin.github"), Kind::Plugin);
    }
}
