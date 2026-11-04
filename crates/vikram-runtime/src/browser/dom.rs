/// Every tag whose content is not page text.
const OPAQUE_ELEMENTS: &[&str] = &["script", "style", "template", "noscript"];

/// Every element that starts a new visual block, so its boundaries become line breaks.
const BLOCK_ELEMENTS: &[&str] = &[
    "address",
    "article",
    "aside",
    "blockquote",
    "body",
    "br",
    "caption",
    "dd",
    "details",
    "div",
    "dl",
    "dt",
    "fieldset",
    "figcaption",
    "figure",
    "footer",
    "form",
    "h1",
    "h2",
    "h3",
    "h4",
    "h5",
    "h6",
    "head",
    "header",
    "hr",
    "html",
    "li",
    "main",
    "nav",
    "ol",
    "option",
    "p",
    "pre",
    "section",
    "summary",
    "table",
    "tbody",
    "td",
    "tfoot",
    "th",
    "thead",
    "title",
    "tr",
    "ul",
];

/// Elements that never have a closing tag.
const VOID_ELEMENTS: &[&str] = &[
    "area", "base", "br", "col", "embed", "hr", "img", "input", "link", "meta", "param", "source",
    "track", "wbr",
];

// ---------------------------------------------------------------------------------------------
// Discovery
// ---------------------------------------------------------------------------------------------

/// Strip tags, comments, scripts and styles and return the readable text of the page.
///
/// Block-level element boundaries become line breaks, every other run of whitespace collapses to
/// one space, and the common HTML entities are decoded. It is a scanner, not a parser: malformed
/// markup costs fidelity, never an error.
pub fn extract_text(dom: &str) -> String {
    let cleaned = strip_non_content(dom);
    let lower = cleaned.to_ascii_lowercase();
    let mut text = String::with_capacity(cleaned.len() / 2);
    let mut cursor = 0usize;
    while let Some(tag) = next_tag(&cleaned, &lower, cursor) {
        if tag.start > cursor {
            text.push_str(&decode_entities(&cleaned[cursor..tag.start]));
        }
        cursor = tag.end + 1;
        if BLOCK_ELEMENTS.contains(&tag.name.as_str()) {
            text.push('\n');
        }
        if !tag.closing && !tag.self_closing && OPAQUE_ELEMENTS.contains(&tag.name.as_str()) {
            cursor = skip_element(&lower, cursor, &tag.name);
        }
    }
    if cursor < cleaned.len() {
        text.push_str(&decode_entities(&cleaned[cursor..]));
    }
    normalise_whitespace(&text)
}

/// Every `<a href="…">text</a>` in the DOM, as `(href, text)` pairs in document order.
///
/// Anchors without an `href` (named anchors) are skipped, as are anchors whose text is empty after
/// tag stripping; a `<a>` with no closing tag ends the scan, because guessing where its text ends
/// would invent links the page does not have.
pub fn extract_links(dom: &str) -> Vec<(String, String)> {
    let cleaned = strip_non_content(dom);
    let lower = cleaned.to_ascii_lowercase();
    let mut links: Vec<(String, String)> = Vec::new();
    let mut cursor = 0usize;
    while let Some(tag) = next_tag(&cleaned, &lower, cursor) {
        cursor = tag.end + 1;
        if tag.closing || tag.name != "a" {
            continue;
        }
        let Some(href) = attr_value(tag.inner, "href") else {
            continue;
        };
        let href = href.trim().to_string();
        if href.is_empty() {
            continue;
        }
        let Some(close_rel) = lower[cursor..].find("</a") else {
            break;
        };
        let close = cursor + close_rel;
        let text = extract_text(&cleaned[cursor..close]);
        if !text.is_empty() {
            links.push((href, text));
        }
        cursor = match lower[close..].find('>') {
            Some(offset) => close + offset + 1,
            None => lower.len(),
        };
    }
    links
}

/// A best-effort description of the page's forms and their controls.
///
/// Each entry is one line: `<form …>` produces `form action=… method=…`, and every `input`,
/// `textarea`, `select` and `button` produces its kind plus the attributes a reader needs
/// (`type`, `name`, `placeholder`). Values with spaces are quoted. Nothing is validated — this is
/// a description for a reader, not a submission.
pub fn extract_forms(dom: &str) -> Vec<String> {
    let cleaned = strip_non_content(dom);
    let lower = cleaned.to_ascii_lowercase();
    let mut forms: Vec<String> = Vec::new();
    let mut cursor = 0usize;
    while let Some(tag) = next_tag(&cleaned, &lower, cursor) {
        cursor = tag.end + 1;
        if tag.closing {
            continue;
        }
        match tag.name.as_str() {
            "form" => forms.push(describe_control("form", tag.inner)),
            "input" | "textarea" | "select" | "button" => {
                forms.push(describe_control(&tag.name, tag.inner));
            }
            _ => {}
        }
    }
    forms
}

/// `kind` plus the attributes that matter for that kind, in a stable order.
fn describe_control(kind: &str, tag: &str) -> String {
    let wanted: &[&str] = match kind {
        "form" => &["action", "method"],
        "input" => &["type", "name", "placeholder"],
        "textarea" => &["name", "placeholder"],
        "select" => &["name"],
        "button" => &["type", "name"],
        _ => &[],
    };
    let mut line = String::from(kind);
    for name in wanted {
        let Some(value) = attr_value(tag, name) else {
            continue;
        };
        let value = value.trim();
        if value.is_empty() {
            continue;
        }
        line.push(' ');
        line.push_str(name);
        line.push('=');
        if value.chars().any(char::is_whitespace) {
            line.push('"');
            line.push_str(value);
            line.push('"');
        } else {
            line.push_str(value);
        }
    }
    line
}

// ---------------------------------------------------------------------------------------------
// A tag scanner
// ---------------------------------------------------------------------------------------------

/// One tag found in the DOM.
struct Tag<'a> {
    /// Lowercased element name, e.g. `a`.
    name: String,
    /// `true` for `</a>`.
    closing: bool,
    /// `true` for `<br>`, `<img src="…">` and `<p/>`.
    self_closing: bool,
    /// The text between the angle brackets, in its original case.
    inner: &'a str,
    /// Byte offset of the opening `<`.
    start: usize,
    /// Byte offset of the closing `>`.
    end: usize,
}

/// The next tag at or after `from`, in a document whose lowercase twin is `lower`.
///
/// `dom` and `lower` must differ only in ASCII case, which keeps a byte offset valid in both: the
/// structure is scanned in `lower` while attribute values are read from `dom` unmangled.
fn next_tag<'a>(dom: &'a str, lower: &str, from: usize) -> Option<Tag<'a>> {
    let mut cursor = from;
    loop {
        if cursor >= lower.len() {
            return None;
        }
        let start = cursor + lower[cursor..].find('<')?;
        if let Some(comment) = lower[start..].strip_prefix("<!--") {
            let offset = comment.find("-->")?;
            cursor = start + "<!--".len() + offset + "-->".len();
            continue;
        }
        let end = start + lower[start..].find('>')?;
        let body = lower[start + 1..end].trim();
        let closing = body.starts_with('/');
        let body = if closing {
            body[1..].trim_start()
        } else {
            body
        };
        let name: String = body
            .chars()
            .take_while(|ch| ch.is_ascii_alphanumeric() || *ch == '-' || *ch == ':')
            .collect();
        if name.is_empty() {
            // `<!doctype html>`, `<?xml …?>`: not an element.
            cursor = end + 1;
            continue;
        }
        let self_closing = body.ends_with('/') || VOID_ELEMENTS.contains(&name.as_str());
        return Some(Tag {
            name,
            closing,
            self_closing,
            inner: &dom[start + 1..end],
            start,
            end,
        });
    }
}

/// The index just past the closing tag of `name`, whose body starts at `from`.
fn skip_element(lower: &str, from: usize, name: &str) -> usize {
    let close = format!("</{name}");
    match lower[from..].find(&close) {
        Some(offset) => {
            let at = from + offset;
            match lower[at..].find('>') {
                Some(gt) => at + gt + 1,
                None => lower.len(),
            }
        }
        None => lower.len(),
    }
}

/// The value of attribute `name` in the text between a tag's angle brackets.
///
/// Handles `name="x"`, `name='x'` and `name=x`, and refuses to match a longer attribute that merely
/// ends in `name` (`data-href` is not `href`).
fn attr_value(tag: &str, name: &str) -> Option<String> {
    let lower = tag.to_ascii_lowercase();
    let mut from = 0usize;
    while let Some(offset) = lower[from..].find(name) {
        let start = from + offset;
        let after = start + name.len();
        let starts_a_word = start == 0
            || !lower.as_bytes()[start - 1].is_ascii_alphanumeric()
                && lower.as_bytes()[start - 1] != b'-'
                && lower.as_bytes()[start - 1] != b'_'
                && lower.as_bytes()[start - 1] != b':';
        let mut index = after;
        while index < lower.len() && lower.as_bytes()[index].is_ascii_whitespace() {
            index += 1;
        }
        if starts_a_word && index < lower.len() && lower.as_bytes()[index] == b'=' {
            index += 1;
            while index < lower.len() && lower.as_bytes()[index].is_ascii_whitespace() {
                index += 1;
            }
            if index < lower.len() {
                let quote = lower.as_bytes()[index];
                if quote == b'"' || quote == b'\'' {
                    let value_start = index + 1;
                    let value_end = match tag[value_start..].find(quote as char) {
                        Some(end) => value_start + end,
                        None => tag.len(),
                    };
                    return Some(tag[value_start..value_end].to_string());
                }
                let mut value_end = index;
                while value_end < tag.len()
                    && !tag.as_bytes()[value_end].is_ascii_whitespace()
                    && tag.as_bytes()[value_end] != b'>'
                {
                    value_end += 1;
                }
                return Some(tag[index..value_end].to_string());
            }
            return Some(String::new());
        }
        from = after;
    }
    None
}

// ---------------------------------------------------------------------------------------------
// Text helpers
// ---------------------------------------------------------------------------------------------

/// Remove every comment, doctype and processing instruction before anything else looks at the DOM.
///
/// Otherwise `<!-- hidden -->` and `<!doctype html>` would surface as page text: they are markup,
/// not content, and neither carries a tag name for the scanners to recognise.
fn strip_non_content(dom: &str) -> String {
    let mut out = String::with_capacity(dom.len());
    let mut rest = dom;
    while let Some(position) = rest.find('<') {
        out.push_str(&rest[..position]);
        let tail = &rest[position..];
        if let Some(comment) = tail.strip_prefix("<!--") {
            match comment.find("-->") {
                Some(end) => {
                    rest = &comment[end + "-->".len()..];
                    continue;
                }
                // An unterminated comment swallows the remainder: half a page of commented-out
                // markup is still commented out.
                None => return out,
            }
        }
        if tail.starts_with("<!") || tail.starts_with("<?") {
            match tail.find('>') {
                Some(end) => {
                    rest = &tail[end + 1..];
                    continue;
                }
                None => return out,
            }
        }
        out.push('<');
        rest = &tail[1..];
    }
    out.push_str(rest);
    out
}

/// Decode the handful of entities a page's text actually uses, plus numeric ones.
fn decode_entities(text: &str) -> String {
    if !text.contains('&') {
        return text.to_string();
    }
    let mut out = String::with_capacity(text.len());
    let mut rest = text;
    while let Some(amp) = rest.find('&') {
        out.push_str(&rest[..amp]);
        let after = &rest[amp..];
        let Some(semi) = after.find(';') else {
            out.push_str(after);
            return out;
        };
        let entity = &after[1..semi];
        match decode_entity(entity) {
            Some(decoded) => {
                out.push_str(&decoded);
                rest = &after[semi + 1..];
            }
            None => {
                out.push('&');
                rest = &after[1..];
            }
        }
    }
    out.push_str(rest);
    out
}

fn decode_entity(entity: &str) -> Option<String> {
    let decoded = match entity {
        "amp" => "&".to_string(),
        "lt" => "<".to_string(),
        "gt" => ">".to_string(),
        "quot" => "\"".to_string(),
        "apos" | "#39" => "'".to_string(),
        "nbsp" => " ".to_string(),
        "mdash" => "—".to_string(),
        "ndash" => "–".to_string(),
        "hellip" => "…".to_string(),
        "copy" => "©".to_string(),
        "middot" => "·".to_string(),
        _ => {
            let code = if let Some(hex) = entity
                .strip_prefix("#x")
                .or_else(|| entity.strip_prefix("#X"))
            {
                u32::from_str_radix(hex, 16).ok()?
            } else {
                entity.strip_prefix('#')?.parse::<u32>().ok()?
            };
            char::from_u32(code)?.to_string()
        }
    };
    Some(decoded)
}

/// Collapse whitespace into readable lines: spaces to one space, any run of line breaks to one.
fn normalise_whitespace(text: &str) -> String {
    let mut out = String::with_capacity(text.len());
    let mut pending_space = false;
    let mut line_breaks = 0usize;
    for ch in text.chars() {
        if ch == '\n' || ch == '\r' {
            line_breaks += 1;
            pending_space = false;
            continue;
        }
        if ch.is_whitespace() {
            pending_space = true;
            continue;
        }
        if line_breaks > 0 {
            if !out.is_empty() {
                out.push('\n');
            }
            line_breaks = 0;
            pending_space = false;
        } else if pending_space && !out.is_empty() {
            out.push(' ');
            pending_space = false;
        }
        out.push(ch);
    }
    out.trim().to_string()
}

// ---------------------------------------------------------------------------------------------
// Interactive sessions: the Chrome DevTools Protocol
// ---------------------------------------------------------------------------------------------
