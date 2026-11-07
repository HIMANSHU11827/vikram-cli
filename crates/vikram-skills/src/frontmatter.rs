//! A hand-written YAML subset reader for `SKILL.md` frontmatter.
//!
//! The subset is what skill files actually use: scalars (plain, single- and double-quoted),
//! `|` and `>` block scalars with chomping, inline `[a, b]` lists, nested maps, nested
//! sequences (scalars and small maps), `#` comments, booleans and integers. Anything outside
//! the subset is kept as a string rather than guessed at, and unknown keys survive a
//! `parse` -> `to_yaml` round trip because entries keep their source order and their parsed
//! value.

use std::path::{Path, PathBuf};

use crate::error::{Result, SkillError};

#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Yaml {
    Null,
    Bool(bool),
    Int(i64),
    Str(String),
    List(Vec<Yaml>),
    Map(Vec<(String, Yaml)>),
}

impl Yaml {
    pub fn as_str(&self) -> Option<&str> {
        match self {
            Yaml::Str(s) => Some(s),
            _ => None,
        }
    }

    pub fn as_i64(&self) -> Option<i64> {
        match self {
            Yaml::Int(i) => Some(*i),
            _ => None,
        }
    }

    pub fn as_bool(&self) -> Option<bool> {
        match self {
            Yaml::Bool(b) => Some(*b),
            _ => None,
        }
    }

    pub fn as_list(&self) -> Option<&[Yaml]> {
        match self {
            Yaml::List(l) => Some(l),
            _ => None,
        }
    }

    pub fn as_map(&self) -> Option<&[(String, Yaml)]> {
        match self {
            Yaml::Map(m) => Some(m),
            _ => None,
        }
    }

    /// Flattens a scalar or a list of scalars into strings. Used for `tags`, `permissions`
    /// and friends, where either spelling is legal in the wild.
    pub fn strings(&self) -> Vec<String> {
        match self {
            Yaml::Null => Vec::new(),
            Yaml::Bool(b) => vec![b.to_string()],
            Yaml::Int(i) => vec![i.to_string()],
            Yaml::Str(s) => vec![s.clone()],
            Yaml::List(items) => items.iter().flat_map(Yaml::strings).collect(),
            Yaml::Map(entries) => entries
                .iter()
                .map(|(k, v)| match v {
                    Yaml::Str(s) => format!("{k}:{s}"),
                    Yaml::Bool(b) => format!("{k}:{b}"),
                    Yaml::Int(i) => format!("{k}:{i}"),
                    _ => k.clone(),
                })
                .collect(),
        }
    }

    pub fn is_null(&self) -> bool {
        matches!(self, Yaml::Null)
    }
}

#[derive(Clone, Debug, PartialEq, Eq, Default)]
pub struct Frontmatter {
    /// Source order is preserved so an unrelated key never moves when one is edited.
    pub entries: Vec<(String, Yaml)>,
    /// Everything after the closing delimiter.
    pub body: String,
    /// The exact frontmatter text between the delimiters, without them.
    pub raw: String,
}

impl Frontmatter {
    /// Parses `---\n<yaml>\n---\n<body>`. Missing frontmatter is not an error.
    pub fn parse(text: &str) -> Result<Frontmatter> {
        Self::parse_at(text, Path::new(""))
    }

    /// Same, with the file path attached to any error. Discovery and the loader use this so a
    /// malformed skill names the file that broke.
    pub(crate) fn parse_at(text: &str, path: &Path) -> Result<Frontmatter> {
        let (raw, body) = split_document(text);
        let entries = match raw {
            Some(block) => parse_entries(block, path)?,
            None => Vec::new(),
        };
        Ok(Frontmatter {
            entries,
            body: body.to_string(),
            raw: raw.unwrap_or("").to_string(),
        })
    }

    pub fn get(&self, key: &str) -> Option<&Yaml> {
        self.entries.iter().find(|(k, _)| k == key).map(|(_, v)| v)
    }

    /// Canonical block re-emit. Scalars stay inline, containers become indented blocks.
    pub fn to_yaml(&self) -> String {
        let mut out = String::new();
        emit_map(&self.entries, 0, &mut out);
        out
    }

    /// Replaces a key in place (keeping its position) or appends it. `raw` is rebuilt so the
    /// two stay consistent.
    pub fn with(&self, key: &str, value: Yaml) -> Frontmatter {
        let mut entries = self.entries.clone();
        match entries.iter_mut().find(|(k, _)| k == key) {
            Some(slot) => slot.1 = value,
            None => entries.push((key.to_string(), value)),
        }
        let next = Frontmatter {
            entries,
            body: self.body.clone(),
            raw: String::new(),
        };
        Frontmatter {
            raw: next.to_yaml(),
            ..next
        }
    }
}

/// Renders a `SKILL.md`: delimiters, frontmatter, body.
pub(crate) fn render_document(fm: &Frontmatter, body: &str) -> String {
    let yaml = fm.to_yaml();
    let mut out = String::from("---\n");
    out.push_str(&yaml);
    if !yaml.is_empty() && !yaml.ends_with('\n') {
        out.push('\n');
    }
    out.push_str("---\n");
    out.push_str(body);
    out
}

/// Splits a document into (frontmatter block, body). A leading `---` with no closing
/// delimiter is markdown (a horizontal rule), not a broken skill: it is treated as body.
fn split_document(text: &str) -> (Option<&str>, &str) {
    let mut lines = text.split_inclusive('\n');
    let Some(first) = lines.next() else {
        return (None, text);
    };
    if first.trim_end_matches(['\r', '\n']).trim() != "---" {
        return (None, text);
    }

    let raw_start = first.len();
    let mut pos = raw_start;
    for line in lines {
        let content = line.trim_end_matches(['\r', '\n']);
        if content.trim() == "---" {
            let raw = text.get(raw_start..pos).unwrap_or("");
            let body_start = pos + line.len();
            let body = text.get(body_start..).unwrap_or("");
            return (Some(raw), body);
        }
        pos += line.len();
    }
    (None, text)
}

/// Parses a standalone YAML document into a single value. Bundles use it; `SKILL.md` uses
/// [`parse_entries`] because its top level must be a mapping.
pub(crate) fn parse_document(text: &str, path: &Path) -> Result<Yaml> {
    let mut block = Block::new(text, path);
    match block.peek()? {
        None => Ok(Yaml::Null),
        Some((_, indent)) => {
            let first = block.lines.get(block.pos).copied().unwrap_or("");
            let content = first.get(indent..).unwrap_or("");
            if is_seq_item(content) {
                block.parse_seq(indent)
            } else {
                Ok(Yaml::Map(block.parse_map(indent)?))
            }
        }
    }
}

fn parse_entries(text: &str, path: &Path) -> Result<Vec<(String, Yaml)>> {
    let mut block = Block::new(text, path);
    let Some((idx, indent)) = block.peek()? else {
        return Ok(Vec::new());
    };
    let line = block.lines.get(idx).copied().unwrap_or("");
    let content = line.get(indent..).unwrap_or("");
    if is_seq_item(content) {
        return Err(SkillError::frontmatter(
            path.to_path_buf(),
            "frontmatter must be a mapping, found a sequence",
        ));
    }
    block.parse_map(indent)
}

struct Block<'a> {
    lines: Vec<&'a str>,
    pos: usize,
    path: PathBuf,
}

impl<'a> Block<'a> {
    fn new(text: &'a str, path: &Path) -> Self {
        Self {
            lines: text.split('\n').collect(),
            pos: 0,
            path: path.to_path_buf(),
        }
    }

    fn err(&self, reason: impl Into<String>) -> SkillError {
        SkillError::frontmatter(self.path.clone(), reason)
    }

    /// Next line that is neither blank nor a whole-line comment, with its indentation.
    fn peek(&self) -> Result<Option<(usize, usize)>> {
        let mut i = self.pos;
        while let Some(line) = self.lines.get(i) {
            let trimmed = line.trim();
            if trimmed.is_empty() || trimmed.starts_with('#') {
                i += 1;
                continue;
            }
            return Ok(Some((i, indent_of(line, &self.path)?)));
        }
        Ok(None)
    }

    fn parse_map(&mut self, indent: usize) -> Result<Vec<(String, Yaml)>> {
        let mut entries = Vec::new();
        let mut seen: Vec<String> = Vec::new();
        self.map_body(indent, &mut entries, &mut seen)?;
        Ok(entries)
    }

    fn map_body(
        &mut self,
        indent: usize,
        entries: &mut Vec<(String, Yaml)>,
        seen: &mut Vec<String>,
    ) -> Result<()> {
        loop {
            let Some((idx, line_indent)) = self.peek()? else {
                return Ok(());
            };
            if line_indent < indent {
                return Ok(());
            }
            if line_indent > indent {
                return Err(self.err(format!("unexpected indentation on line {}", idx + 1)));
            }
            let line = self.lines.get(idx).copied().unwrap_or("");
            let content = line.get(indent..).unwrap_or("");
            if is_seq_item(content) {
                return Ok(());
            }
            self.pos = idx + 1;
            let Some((raw_key, rest)) = split_key(content) else {
                return Err(self.err(format!(
                    "expected `key: value` on line {}, found `{}`",
                    idx + 1,
                    content.trim()
                )));
            };
            let key = parse_key(raw_key, &self.path)?;
            if seen.iter().any(|k| k == &key) {
                return Err(self.err(format!("duplicate key `{key}`")));
            }
            seen.push(key.clone());
            let value = self.parse_value(rest, indent)?;
            entries.push((key, value));
        }
    }

    fn parse_value(&mut self, rest: &str, parent_indent: usize) -> Result<Yaml> {
        let raw = rest.trim_start();
        let trimmed = strip_comment(raw).trim_end();
        if trimmed.is_empty() {
            return self.continuation(parent_indent);
        }
        match trimmed.chars().next() {
            Some('|') | Some('>') => self.block_scalar(trimmed, parent_indent),
            Some('[') => parse_inline_list(trimmed, &self.path),
            _ => parse_scalar(trimmed, &self.path),
        }
    }

    /// A key with nothing after the colon: either a nested block or an explicit null.
    fn continuation(&mut self, parent_indent: usize) -> Result<Yaml> {
        match self.peek()? {
            Some((idx, indent)) if indent > parent_indent => {
                let line = self.lines.get(idx).copied().unwrap_or("");
                let content = line.get(indent..).unwrap_or("");
                if is_seq_item(content) {
                    self.parse_seq(indent)
                } else {
                    Ok(Yaml::Map(self.parse_map(indent)?))
                }
            }
            _ => Ok(Yaml::Null),
        }
    }

    fn parse_seq(&mut self, indent: usize) -> Result<Yaml> {
        let mut items = Vec::new();
        while let Some((idx, line_indent)) = self.peek()? {
            if line_indent != indent {
                break;
            }
            let line = self.lines.get(idx).copied().unwrap_or("");
            let content = line.get(indent..).unwrap_or("");
            if !is_seq_item(content) {
                break;
            }
            self.pos = idx + 1;
            let rest = content.get(1..).unwrap_or("");
            if rest.trim().is_empty() {
                items.push(self.continuation(indent)?);
                continue;
            }
            let after = rest.trim_start();
            let item_indent = indent + 1 + (rest.len() - after.len());
            if let Some((raw_key, value_rest)) = split_key(after)
                && !raw_key.trim().is_empty()
            {
                items.push(Yaml::Map(self.map_item(
                    raw_key,
                    value_rest,
                    item_indent,
                )?));
                continue;
            }
            let trimmed = strip_comment(after).trim_end();
            if trimmed.is_empty() {
                items.push(self.continuation(indent)?);
                continue;
            }
            match trimmed.chars().next() {
                Some('|') | Some('>') => items.push(self.block_scalar(trimmed, indent)?),
                Some('[') => items.push(parse_inline_list(trimmed, &self.path)?),
                _ => items.push(parse_scalar(trimmed, &self.path)?),
            }
        }
        Ok(Yaml::List(items))
    }

    /// `- key: value` plus the following keys aligned under `key`.
    fn map_item(
        &mut self,
        raw_key: &str,
        value_rest: &str,
        item_indent: usize,
    ) -> Result<Vec<(String, Yaml)>> {
        let key = parse_key(raw_key, &self.path)?;
        let value = self.parse_value(value_rest, item_indent)?;
        let mut entries = vec![(key.clone(), value)];
        let mut seen = vec![key];
        self.map_body(item_indent, &mut entries, &mut seen)?;
        Ok(entries)
    }

    fn block_scalar(&mut self, header: &str, parent_indent: usize) -> Result<Yaml> {
        let fold = header.starts_with('>');
        let mut chomp = Chomp::Clip;
        for c in header.chars() {
            match c {
                '-' => chomp = Chomp::Strip,
                '+' => chomp = Chomp::Keep,
                _ => {}
            }
        }

        let mut content: Vec<String> = Vec::new();
        let mut content_indent: Option<usize> = None;
        let mut i = self.pos;
        // `text.split('\n')` leaves one empty tail element when the block ends with a newline.
        // That element is the terminator, not a blank line inside the scalar.
        let phantom = self.lines.len().saturating_sub(1);
        while let Some(line) = self.lines.get(i) {
            if line.trim().is_empty() {
                if i == phantom {
                    i += 1;
                    break;
                }
                content.push(String::new());
                i += 1;
                continue;
            }
            let indent = indent_of(line, &self.path)?;
            let ci = match content_indent {
                Some(ci) => ci,
                None => {
                    if indent <= parent_indent {
                        break;
                    }
                    content_indent = Some(indent);
                    indent
                }
            };
            if indent < ci {
                break;
            }
            content.push(line.get(ci..).unwrap_or("").trim_end().to_string());
            i += 1;
        }
        self.pos = i;

        let has_content = content.iter().any(|l| !l.is_empty());
        let mut text = if fold {
            fold_lines(&content)
        } else {
            content.join("\n")
        };
        let text = match chomp {
            Chomp::Strip => text.trim_end_matches('\n').to_string(),
            Chomp::Keep => {
                if has_content {
                    text.push('\n');
                }
                text
            }
            Chomp::Clip => {
                if has_content {
                    format!("{}\n", text.trim_end_matches('\n'))
                } else {
                    String::new()
                }
            }
        };
        Ok(Yaml::Str(text))
    }
}

#[derive(Clone, Copy)]
enum Chomp {
    Strip,
    Clip,
    Keep,
}

fn indent_of(line: &str, path: &Path) -> Result<usize> {
    let mut n = 0;
    for c in line.chars() {
        match c {
            ' ' => n += 1,
            '\t' => {
                return Err(SkillError::frontmatter(
                    path.to_path_buf(),
                    "tab characters are not allowed in indentation",
                ));
            }
            _ => break,
        }
    }
    Ok(n)
}

fn is_seq_item(content: &str) -> bool {
    content == "-" || content.starts_with("- ") || content == "-\t" || content.starts_with("-\t")
}

fn split_key(s: &str) -> Option<(&str, &str)> {
    let bytes = s.as_bytes();
    let mut in_single = false;
    let mut in_double = false;
    for (i, byte) in bytes.iter().enumerate() {
        match byte {
            b'\'' if !in_double => in_single = !in_single,
            b'"' if !in_single => in_double = !in_double,
            b':' if !in_single && !in_double => {
                let next = bytes.get(i + 1);
                if next.is_none() || matches!(next, Some(b' ' | b'\t')) {
                    return Some((s.get(..i)?, s.get(i + 1..)?));
                }
            }
            _ => {}
        }
    }
    None
}

fn parse_key(raw: &str, path: &Path) -> Result<String> {
    let trimmed = raw.trim();
    if trimmed.is_empty() {
        return Err(SkillError::frontmatter(
            path.to_path_buf(),
            "empty mapping key",
        ));
    }
    if trimmed.starts_with('"') || trimmed.starts_with('\'') {
        return match parse_scalar(trimmed, path)? {
            Yaml::Str(s) => Ok(s),
            _ => Ok(trimmed.to_string()),
        };
    }
    Ok(trimmed.to_string())
}

fn parse_scalar(s: &str, path: &Path) -> Result<Yaml> {
    if let Some(rest) = s.strip_prefix('"') {
        return Ok(Yaml::Str(unquote_double(rest, path)?));
    }
    if let Some(rest) = s.strip_prefix('\'') {
        return Ok(Yaml::Str(unquote_single(rest, path)?));
    }
    Ok(infer_plain(s))
}

fn infer_plain(s: &str) -> Yaml {
    let t = s.trim();
    if t.is_empty() {
        return Yaml::Null;
    }
    if t == "~" {
        return Yaml::Null;
    }
    match t.to_ascii_lowercase().as_str() {
        "true" | "yes" => return Yaml::Bool(true),
        "false" | "no" => return Yaml::Bool(false),
        "null" => return Yaml::Null,
        _ => {}
    }
    if let Ok(i) = t.parse::<i64>()
        && t.chars()
            .all(|c| c.is_ascii_digit() || c == '+' || c == '-')
    {
        return Yaml::Int(i);
    }
    Yaml::Str(t.to_string())
}

fn unquote_double(rest: &str, path: &Path) -> Result<String> {
    let mut out = String::new();
    let mut chars = rest.char_indices();
    let mut closed_at: Option<usize> = None;
    while let Some((i, c)) = chars.next() {
        match c {
            '"' => {
                closed_at = Some(i);
                break;
            }
            '\\' => {
                let Some((_, esc)) = chars.next() else {
                    return Err(SkillError::frontmatter(
                        path.to_path_buf(),
                        "unterminated escape in double-quoted scalar",
                    ));
                };
                match esc {
                    'n' => out.push('\n'),
                    'r' => out.push('\r'),
                    't' => out.push('\t'),
                    '0' => out.push('\0'),
                    '\\' => out.push('\\'),
                    '"' => out.push('"'),
                    '/' => out.push('/'),
                    'u' => {
                        let mut hex = String::new();
                        for _ in 0..4 {
                            match chars.next() {
                                Some((_, h)) => hex.push(h),
                                None => {
                                    return Err(SkillError::frontmatter(
                                        path.to_path_buf(),
                                        "truncated \\u escape",
                                    ));
                                }
                            }
                        }
                        let value = u32::from_str_radix(&hex, 16).map_err(|_| {
                            SkillError::frontmatter(
                                path.to_path_buf(),
                                format!("invalid \\u escape `{hex}`"),
                            )
                        })?;
                        match char::from_u32(value) {
                            Some(ch) => out.push(ch),
                            None => {
                                return Err(SkillError::frontmatter(
                                    path.to_path_buf(),
                                    format!("invalid code point `{hex}`"),
                                ));
                            }
                        }
                    }
                    other => {
                        out.push('\\');
                        out.push(other);
                    }
                }
            }
            other => out.push(other),
        }
    }
    let Some(closed) = closed_at else {
        return Err(SkillError::frontmatter(
            path.to_path_buf(),
            "unterminated double-quoted scalar",
        ));
    };
    let tail = rest.get(closed + 1..).unwrap_or("");
    check_trailing(tail, path)?;
    Ok(out)
}

fn unquote_single(rest: &str, path: &Path) -> Result<String> {
    let mut out = String::new();
    let mut chars = rest.char_indices().peekable();
    let mut closed_at: Option<usize> = None;
    while let Some((i, c)) = chars.next() {
        if c != '\'' {
            out.push(c);
            continue;
        }
        match chars.peek() {
            Some((_, '\'')) => {
                out.push('\'');
                chars.next();
            }
            _ => {
                closed_at = Some(i);
                break;
            }
        }
    }
    let Some(closed) = closed_at else {
        return Err(SkillError::frontmatter(
            path.to_path_buf(),
            "unterminated single-quoted scalar",
        ));
    };
    let tail = rest.get(closed + 1..).unwrap_or("");
    check_trailing(tail, path)?;
    Ok(out)
}

fn check_trailing(tail: &str, path: &Path) -> Result<()> {
    let t = tail.trim();
    if t.is_empty() || t.starts_with('#') {
        return Ok(());
    }
    Err(SkillError::frontmatter(
        path.to_path_buf(),
        format!("unexpected content after quoted scalar: `{t}`"),
    ))
}

fn strip_comment(s: &str) -> &str {
    let bytes = s.as_bytes();
    let mut in_single = false;
    let mut in_double = false;
    let mut prev_space = true;
    for (i, byte) in bytes.iter().enumerate() {
        match byte {
            b'\'' if !in_double => in_single = !in_single,
            b'"' if !in_single => in_double = !in_double,
            b'#' if !in_single && !in_double && prev_space => {
                return s.get(..i).unwrap_or("");
            }
            _ => {}
        }
        prev_space = matches!(byte, b' ' | b'\t');
    }
    s
}

fn parse_inline_list(s: &str, path: &Path) -> Result<Yaml> {
    let inner = s
        .strip_prefix('[')
        .ok_or_else(|| SkillError::frontmatter(path.to_path_buf(), "expected `[`"))?;
    let inner = inner.trim_end();
    let inner = inner
        .strip_suffix(']')
        .ok_or_else(|| SkillError::frontmatter(path.to_path_buf(), "unterminated inline list"))?;
    let inner = inner.trim();
    if inner.is_empty() {
        return Ok(Yaml::List(Vec::new()));
    }
    let mut items = Vec::new();
    for part in split_top_level(inner) {
        let p = part.trim();
        if p.is_empty() {
            continue;
        }
        if p.starts_with('[') {
            items.push(parse_inline_list(p, path)?);
        } else {
            items.push(parse_scalar(p, path)?);
        }
    }
    Ok(Yaml::List(items))
}

fn split_top_level(s: &str) -> Vec<&str> {
    let mut out = Vec::new();
    let mut depth = 0usize;
    let mut in_single = false;
    let mut in_double = false;
    let mut start = 0usize;
    for (i, c) in s.char_indices() {
        match c {
            '\'' if !in_double => in_single = !in_single,
            '"' if !in_single => in_double = !in_double,
            '[' | '{' if !in_single && !in_double => depth += 1,
            ']' | '}' if !in_single && !in_double => depth = depth.saturating_sub(1),
            ',' if depth == 0 && !in_single && !in_double => {
                out.push(s.get(start..i).unwrap_or(""));
                start = i + 1;
            }
            _ => {}
        }
    }
    out.push(s.get(start..).unwrap_or(""));
    out
}

fn fold_lines(lines: &[String]) -> String {
    let mut out = String::new();
    let mut prev_blank = true;
    for (i, line) in lines.iter().enumerate() {
        if i == 0 {
            out.push_str(line);
        } else if line.is_empty() || prev_blank {
            out.push('\n');
            out.push_str(line);
        } else {
            out.push(' ');
            out.push_str(line);
        }
        prev_blank = line.is_empty();
    }
    out
}

fn pad(n: usize) -> String {
    " ".repeat(n)
}

fn emit_map(entries: &[(String, Yaml)], indent: usize, out: &mut String) {
    for (key, value) in entries {
        out.push_str(&pad(indent));
        emit_kv(key, value, indent, out);
    }
}

fn emit_kv(key: &str, value: &Yaml, key_indent: usize, out: &mut String) {
    out.push_str(&quote_if_needed(key));
    out.push(':');
    match value {
        Yaml::Map(m) if m.is_empty() => out.push_str(" {}\n"),
        Yaml::List(l) if l.is_empty() => out.push_str(" []\n"),
        Yaml::Map(m) => {
            out.push('\n');
            emit_map(m, key_indent + 2, out);
        }
        Yaml::List(l) => {
            out.push('\n');
            emit_seq(l, key_indent + 2, out);
        }
        scalar => {
            out.push(' ');
            out.push_str(&inline_scalar(scalar));
            out.push('\n');
        }
    }
}

fn emit_seq(items: &[Yaml], indent: usize, out: &mut String) {
    for item in items {
        match item {
            Yaml::Map(m) if !m.is_empty() => {
                out.push_str(&pad(indent));
                out.push_str("-\n");
                emit_map(m, indent + 2, out);
            }
            Yaml::List(l) if !l.is_empty() => {
                out.push_str(&pad(indent));
                out.push_str("-\n");
                emit_seq(l, indent + 2, out);
            }
            scalar => {
                out.push_str(&pad(indent));
                out.push_str("- ");
                out.push_str(&inline_scalar(scalar));
                out.push('\n');
            }
        }
    }
}

fn inline_scalar(value: &Yaml) -> String {
    match value {
        Yaml::Null => "null".to_string(),
        Yaml::Bool(b) => b.to_string(),
        Yaml::Int(i) => i.to_string(),
        Yaml::Str(s) => quote_if_needed(s),
        Yaml::List(_) => "[]".to_string(),
        Yaml::Map(_) => "{}".to_string(),
    }
}

fn quote_if_needed(s: &str) -> String {
    if s.is_empty() || needs_quoting(s) {
        let mut out = String::from("\"");
        for c in s.chars() {
            match c {
                '"' => out.push_str("\\\""),
                '\\' => out.push_str("\\\\"),
                '\n' => out.push_str("\\n"),
                '\r' => out.push_str("\\r"),
                '\t' => out.push_str("\\t"),
                other => out.push(other),
            }
        }
        out.push('"');
        return out;
    }
    s.to_string()
}

fn needs_quoting(s: &str) -> bool {
    if s != s.trim() {
        return true;
    }
    if s == "~" {
        return true;
    }
    match s.to_ascii_lowercase().as_str() {
        "true" | "false" | "yes" | "no" | "null" => return true,
        _ => {}
    }
    if s.parse::<i64>().is_ok() {
        return true;
    }
    let first = s.chars().next().unwrap_or(' ');
    if "-?:,[]{}#&*!|>'\"%@`".contains(first) {
        return true;
    }
    let chars: Vec<char> = s.chars().collect();
    for (i, c) in chars.iter().enumerate() {
        if *c == ':' && chars.get(i + 1).map(|n| *n == ' ').unwrap_or(true) {
            return true;
        }
        if *c == '#' && i > 0 && chars.get(i - 1) == Some(&' ') {
            return true;
        }
    }
    false
}

#[cfg(test)]
mod tests {
    use super::*;

    fn fm(text: &str) -> Frontmatter {
        Frontmatter::parse(text).expect("parse")
    }

    #[test]
    fn missing_frontmatter_is_the_whole_document_as_body() {
        let f = fm("# Title\n\nText");
        assert!(f.entries.is_empty());
        assert_eq!(f.body, "# Title\n\nText");
        assert_eq!(f.raw, "");
    }

    #[test]
    fn unterminated_delimiter_is_treated_as_markdown_rule() {
        let f = fm("---\nnot a skill, just a horizontal rule");
        assert!(f.entries.is_empty());
        assert!(f.body.starts_with("---\n"));
    }

    #[test]
    fn parses_a_basic_mapping_and_keeps_the_body() {
        let f = fm("---\nname: check-deps\ndescription: Verify deps\n---\n# Body\n");
        assert_eq!(
            f.entries,
            vec![
                ("name".to_string(), Yaml::Str("check-deps".to_string())),
                (
                    "description".to_string(),
                    Yaml::Str("Verify deps".to_string())
                ),
            ]
        );
        assert_eq!(f.body, "# Body\n");
        assert!(f.raw.contains("name: check-deps"));
    }

    #[test]
    fn parses_quoted_scalars_with_escapes() {
        let f = fm("---\na: \"x\\ny\"\nb: 'it''s'\nc: \"tab\\there\"\n---\n");
        assert_eq!(f.get("a"), Some(&Yaml::Str("x\ny".into())));
        assert_eq!(f.get("b"), Some(&Yaml::Str("it's".into())));
        assert_eq!(f.get("c"), Some(&Yaml::Str("tab\there".into())));
    }

    #[test]
    fn unterminated_quote_is_an_error() {
        let err = Frontmatter::parse("---\na: \"oops\n---\n").expect_err("malformed");
        assert!(err.to_string().contains("unterminated"));
    }

    #[test]
    fn parses_inline_lists_and_empty_lists() {
        let f = fm("---\ntags: [rust, cli, \"two words\"]\nnone: []\n---\n");
        assert_eq!(
            f.get("tags").map(Yaml::strings),
            Some(vec!["rust".into(), "cli".into(), "two words".into()])
        );
        assert_eq!(f.get("none"), Some(&Yaml::List(vec![])));
    }

    #[test]
    fn parses_indented_scalar_lists() {
        let f = fm("---\ntags:\n  - rust\n  - cli\n---\n");
        assert_eq!(
            f.get("tags").map(Yaml::strings),
            Some(vec!["rust".into(), "cli".into()])
        );
    }

    #[test]
    fn parses_lists_of_maps() {
        let f = fm(
            "---\nrequires:\n  - name: a\n    version: '1.2'\n  - name: b\n    version: '2.0'\n---\n",
        );
        let list = f.get("requires").and_then(Yaml::as_list).expect("list");
        assert_eq!(list.len(), 2);
        let first = list[0].as_map().expect("map");
        assert_eq!(first[0], ("name".to_string(), Yaml::Str("a".into())));
        assert_eq!(first[1], ("version".to_string(), Yaml::Str("1.2".into())));
    }

    #[test]
    fn parses_nested_maps() {
        let f = fm("---\nvikram:\n  tags: [a, b]\n  activation: auto\n  requires:\n    - x\n---\n");
        let map = f.get("vikram").and_then(Yaml::as_map).expect("map");
        assert_eq!(map.len(), 3);
        assert_eq!(map[2].0, "requires");
        assert_eq!(map[2].1.strings(), vec!["x".to_string()]);
    }

    #[test]
    fn block_scalar_literal_clips_by_default() {
        let f = fm("---\nbody: |\n  line one\n  line two\nafter: 1\n---\n");
        assert_eq!(
            f.get("body"),
            Some(&Yaml::Str("line one\nline two\n".into()))
        );
        assert_eq!(f.get("after"), Some(&Yaml::Int(1)));
    }

    #[test]
    fn block_scalar_strip_and_keep_chomping() {
        let strip = fm("---\nbody: |-\n  x\n  y\n---\n");
        assert_eq!(strip.get("body"), Some(&Yaml::Str("x\ny".into())));
        let keep = fm("---\nbody: |+\n  x\n\n---\n");
        assert_eq!(keep.get("body"), Some(&Yaml::Str("x\n\n".into())));
    }

    #[test]
    fn block_scalar_folded_joins_lines_with_spaces() {
        let f = fm("---\nbody: >\n  one\n  two\n---\n");
        assert_eq!(f.get("body"), Some(&Yaml::Str("one two\n".into())));
    }

    #[test]
    fn comments_are_stripped_outside_quotes() {
        let f = fm("---\n# leading comment\nname: x # trailing\nquoted: \"a # b\"\n---\n");
        assert_eq!(f.get("name"), Some(&Yaml::Str("x".into())));
        assert_eq!(f.get("quoted"), Some(&Yaml::Str("a # b".into())));
        assert_eq!(f.entries.len(), 2);
    }

    #[test]
    fn booleans_integers_and_nulls_are_typed() {
        let f = fm("---\na: yes\nb: no\nc: true\nd: 42\ne: ~\nf: 3.5\n---\n");
        assert_eq!(f.get("a"), Some(&Yaml::Bool(true)));
        assert_eq!(f.get("b"), Some(&Yaml::Bool(false)));
        assert_eq!(f.get("c"), Some(&Yaml::Bool(true)));
        assert_eq!(f.get("d"), Some(&Yaml::Int(42)));
        assert_eq!(f.get("e"), Some(&Yaml::Null));
        assert_eq!(f.get("f"), Some(&Yaml::Str("3.5".into())));
    }

    #[test]
    fn duplicate_keys_are_an_error() {
        let err = Frontmatter::parse("---\nname: a\nname: b\n---\n").expect_err("duplicate");
        assert!(err.to_string().contains("duplicate key `name`"));
    }

    #[test]
    fn tabs_in_indentation_are_an_error() {
        let err = Frontmatter::parse("---\nvikram:\n\ttags: [a]\n---\n").expect_err("tab");
        assert!(err.to_string().contains("tab"));
    }

    #[test]
    fn a_top_level_sequence_is_an_error() {
        let err = Frontmatter::parse("---\n- a\n- b\n---\n").expect_err("sequence");
        assert!(err.to_string().contains("mapping"));
    }

    #[test]
    fn unknown_keys_survive_a_round_trip() {
        let source = "---\nname: x\nweird:\n  nested: [1, 2]\n  flags:\n    - yes\n    - no\nempty_list: []\nempty_map: {}\nquoted: \": leading colon\"\n---\n";
        let first = fm(source);
        let yaml = first.to_yaml();
        let second = Frontmatter::parse(&format!("---\n{yaml}---\n")).expect("reparse");
        assert_eq!(first.entries, second.entries);
        let weird = second.get("weird").and_then(Yaml::as_map).expect("map");
        assert_eq!(weird[0].1.strings(), vec!["1".to_string(), "2".to_string()]);
        assert_eq!(
            weird[1].1.strings(),
            vec!["true".to_string(), "false".to_string()]
        );
    }

    #[test]
    fn round_trip_preserves_scalars_that_look_like_other_types() {
        let source =
            "---\nname: x\nliteral_number: \"42\"\nliteral_bool: \"yes\"\nspaced: \" a \"\n---\n";
        let first = fm(source);
        let yaml = first.to_yaml();
        let second = Frontmatter::parse(&format!("---\n{yaml}---\n")).expect("reparse");
        assert_eq!(second.get("literal_number"), Some(&Yaml::Str("42".into())));
        assert_eq!(second.get("literal_bool"), Some(&Yaml::Str("yes".into())));
        assert_eq!(second.get("spaced"), Some(&Yaml::Str(" a ".into())));
        assert_eq!(first.entries, second.entries);
    }

    #[test]
    fn with_replaces_in_place_and_appends_new_keys() {
        let f = fm("---\nname: x\ntags: [a]\n---\nbody");
        let replaced = f.with("name", Yaml::Str("y".into()));
        assert_eq!(replaced.entries[0].0, "name");
        assert_eq!(replaced.get("name"), Some(&Yaml::Str("y".into())));
        assert_eq!(replaced.entries.len(), 2);
        assert_eq!(replaced.body, "body");
        assert!(replaced.raw.contains("name: y"));

        let appended = f.with("extra", Yaml::Int(7));
        assert_eq!(
            appended.entries.last().map(|(k, _)| k.as_str()),
            Some("extra")
        );
        assert_eq!(appended.entries.len(), 3);
    }

    #[test]
    fn crlf_documents_parse() {
        let f = fm("---\r\nname: x\r\n---\r\nbody\r\n");
        assert_eq!(f.get("name"), Some(&Yaml::Str("x".into())));
        assert_eq!(f.body, "body\r\n");
    }

    #[test]
    fn parse_at_attaches_the_path_to_errors() {
        let err = Frontmatter::parse_at("---\na: 1\na: 2\n---\n", Path::new("/s/SKILL.md"))
            .expect_err("duplicate");
        assert!(err.to_string().contains("/s/SKILL.md"));
    }

    #[test]
    fn render_document_round_trips_through_parse() {
        let f = fm("---\nname: x\ndescription: d\n---\n# Body\n");
        let text = render_document(&f, &f.body);
        let again = Frontmatter::parse(&text).expect("reparse");
        assert_eq!(again.entries, f.entries);
        assert_eq!(again.body, "# Body\n");
    }

    #[test]
    fn parse_document_reads_a_bundle_file() {
        let doc = parse_document(
            "name: core\nskills:\n  - a\n  - b\noptional: [c]\n",
            Path::new("b.yaml"),
        )
        .expect("doc");
        let map = doc.as_map().expect("map");
        assert_eq!(map[0].0, "name");
        assert_eq!(map[1].1.strings(), vec!["a".to_string(), "b".to_string()]);
    }

    #[test]
    fn yaml_strings_flattens_lists_and_scalars() {
        assert_eq!(Yaml::Str("a".into()).strings(), vec!["a".to_string()]);
        assert_eq!(
            Yaml::List(vec![Yaml::Str("a".into()), Yaml::Int(2)]).strings(),
            vec!["a".to_string(), "2".to_string()]
        );
        assert!(Yaml::Null.strings().is_empty());
    }

    #[test]
    fn a_scalar_with_a_colon_but_no_space_stays_a_string() {
        let f = fm("---\nurl: http://example.com/x\n---\n");
        assert_eq!(
            f.get("url"),
            Some(&Yaml::Str("http://example.com/x".into()))
        );
    }
}
