//! A lightweight, dependency-free Markdown renderer.
//!
//! The goal is a transcript that reads well in a terminal, not a complete CommonMark
//! implementation. Rendering is pure: the same text, width and theme always produce the same
//! lines. Every style comes from [`Theme`], so a plain or colourless terminal degrades
//! predictably instead of leaking escape codes into the transcript.
//!
//! What is handled: fenced code blocks, headings, inline code/bold/italic, unordered and ordered
//! lists, blockquotes, simple pipe tables, ANSI stripping and hard wrapping by display width.

use ratatui::style::{Modifier, Style};
use ratatui::text::{Line, Span};
use unicode_width::{UnicodeWidthChar, UnicodeWidthStr};

use crate::theme::Theme;

/// Inline nesting beyond this depth is treated as literal text; a transcript never needs more.
const MAX_INLINE_DEPTH: usize = 3;

/// One styled fragment of a rendered line.
#[derive(Clone, Debug)]
struct Piece {
    text: String,
    style: Style,
}

impl Piece {
    fn new(text: impl Into<String>, style: Style) -> Self {
        Self {
            text: text.into(),
            style,
        }
    }
}

/// Render Markdown to styled terminal lines no wider than `width` display columns.
pub fn markdown_lines(text: &str, width: usize, theme: &Theme) -> Vec<Line<'static>> {
    markdown_lines_styled(text, width, theme, theme.body())
}

/// Render Markdown with a caller-supplied base style, used so a message can be tinted (for
/// example a user message) without losing heading emphasis.
pub(crate) fn markdown_lines_styled(
    text: &str,
    width: usize,
    theme: &Theme,
    base: Style,
) -> Vec<Line<'static>> {
    let clean = strip_ansi(text);
    let width = width.max(1);
    let rows: Vec<&str> = clean.split('\n').collect();
    let mut out: Vec<Line<'static>> = Vec::new();
    let mut pushed_blank = false;
    let mut index = 0usize;

    while index < rows.len() {
        let row = rows[index];
        let trimmed = row.trim();

        // ---- fenced code -------------------------------------------------
        if let Some((fence, language)) = fence_open(trimmed) {
            let mut body: Vec<String> = Vec::new();
            index += 1;
            while index < rows.len() {
                if rows[index].trim().starts_with(fence) {
                    index += 1;
                    break;
                }
                body.push(rows[index].to_owned());
                index += 1;
            }
            out.extend(code_block(language, &body, width, theme));
            pushed_blank = false;
            continue;
        }

        // ---- table: a piped row followed by a dash separator --------------
        if trimmed.contains('|') && index + 1 < rows.len() && is_table_separator(rows[index + 1]) {
            let mut table: Vec<Vec<String>> = vec![split_row(row)];
            index += 2;
            while index < rows.len() && rows[index].contains('|') && !rows[index].trim().is_empty()
            {
                table.push(split_row(rows[index]));
                index += 1;
            }
            out.extend(table_rows(&table, width, theme, base));
            pushed_blank = false;
            continue;
        }

        // ---- blank -------------------------------------------------------
        if trimmed.is_empty() {
            if !pushed_blank {
                out.push(Line::from(""));
                pushed_blank = true;
            }
            index += 1;
            continue;
        }

        // ---- heading -----------------------------------------------------
        if let Some((level, content)) = heading(trimmed) {
            let style = if level == 1 {
                theme.title()
            } else {
                Style::default().fg(theme.accent)
            };
            out.extend(plain(inline(content, style, theme), width));
            pushed_blank = false;
            index += 1;
            continue;
        }

        // ---- blockquote --------------------------------------------------
        if let Some(rest) = trimmed.strip_prefix('>') {
            let content = rest.strip_prefix(' ').unwrap_or(rest);
            let gutter = if theme.ascii { "| " } else { "│ " };
            let style = theme.secondary();
            let mut pieces = vec![Piece::new(gutter, theme.muted())];
            pieces.extend(inline(content, style, theme));
            out.extend(plain(pieces, width));
            pushed_blank = false;
            index += 1;
            continue;
        }

        // ---- list --------------------------------------------------------
        if let Some(pieces) = list_item(row, theme) {
            out.extend(plain(pieces, width));
            pushed_blank = false;
            index += 1;
            continue;
        }

        // ---- paragraph ---------------------------------------------------
        out.extend(plain(inline(trimmed, base, theme), width));
        pushed_blank = false;
        index += 1;
    }

    // Trailing blank lines carry no information and would only pad a transcript.
    while out.last().is_some_and(line_is_blank) {
        out.pop();
    }
    out
}

/// Wrap plain text (no Markdown) to `width`, one line per source line.
pub(crate) fn wrap_plain(text: &str, width: usize, style: Style) -> Vec<Line<'static>> {
    let clean = strip_ansi(text);
    let mut out: Vec<Line<'static>> = Vec::new();
    for row in clean.split('\n') {
        out.extend(render(
            vec![Piece::new(row.to_owned(), style)],
            Vec::new(),
            Vec::new(),
            width.max(1),
            true,
        ));
    }
    out
}

// ---------------------------------------------------------------------------
// blocks
// ---------------------------------------------------------------------------

fn code_block(language: &str, body: &[String], width: usize, theme: &Theme) -> Vec<Line<'static>> {
    let gutter = if theme.ascii { "|" } else { "▏" };
    let prefix = vec![
        Piece::new(gutter, theme.muted()),
        Piece::new(" ", theme.muted()),
    ];
    // Code keeps a dim base so unrecognised tokens and unknown languages cannot bleed into the
    // body; a known language only recolours comments, strings and keywords on top of it.
    let code_style = theme.secondary();
    let spec = language_spec(language);
    let mut state = CodeState::Normal;

    let mut out: Vec<Line<'static>> = Vec::new();
    if !language.is_empty() {
        out.extend(render(
            vec![Piece::new(language, theme.secondary())],
            prefix.clone(),
            prefix.clone(),
            width,
            false,
        ));
    }
    for line in body {
        let pieces = match &spec {
            Some(spec) => code_pieces(line, spec, &mut state, code_style, theme),
            None => vec![Piece::new(line.clone(), code_style)],
        };
        out.extend(render(pieces, prefix.clone(), prefix.clone(), width, false));
    }
    // A closing gutter line marks the end of the block.
    out.extend(render(
        Vec::new(),
        prefix.clone(),
        prefix.clone(),
        width,
        false,
    ));
    out
}

// ---------------------------------------------------------------------------
// code highlighting
// ---------------------------------------------------------------------------
//
// A deliberately small lexer: comments, strings and keywords for the languages a coding agent
// actually emits. Anything else stays the dim base colour, so highlighting is always a subset of
// the truth — a missed token reads as ordinary code, never as a wrong colour on the wrong thing.

/// What a stretch of code is, as far as colour is concerned.
#[derive(Clone, Copy, PartialEq, Eq)]
enum CodeMode {
    Code,
    Keyword,
    Str,
    Comment,
}

/// What carries across a line boundary inside a fenced block.
#[derive(Clone, Copy, PartialEq, Eq)]
enum CodeState {
    Normal,
    BlockComment,
    /// A Python-style triple-quoted string; the byte is the quote character.
    TripleString(u8),
}

/// The comment and string syntax plus keyword set for one language family.
#[derive(Clone, Copy)]
struct CodeSpec {
    line_comments: &'static [&'static str],
    block_comment: Option<(&'static str, &'static str)>,
    quotes: &'static [u8],
    /// Python `"""`/`'''` strings; only meaningful with `quotes` covering both quote chars.
    triple_quotes: bool,
    keywords: &'static [&'static str],
}

const RUST_KEYWORDS: &[&str] = &[
    "as", "async", "await", "break", "const", "continue", "crate", "dyn", "else", "enum", "extern",
    "false", "fn", "for", "if", "impl", "in", "let", "loop", "match", "mod", "move", "mut", "pub",
    "ref", "return", "self", "Self", "static", "struct", "super", "trait", "true", "type",
    "unsafe", "use", "where", "while",
];
const PYTHON_KEYWORDS: &[&str] = &[
    "False", "None", "True", "and", "as", "assert", "async", "await", "break", "class", "continue",
    "def", "del", "elif", "else", "except", "finally", "for", "from", "global", "if", "import",
    "in", "is", "lambda", "nonlocal", "not", "or", "pass", "raise", "return", "try", "while",
    "with", "yield",
];
const JS_KEYWORDS: &[&str] = &[
    "async",
    "await",
    "break",
    "case",
    "catch",
    "class",
    "const",
    "continue",
    "debugger",
    "default",
    "delete",
    "do",
    "else",
    "export",
    "extends",
    "false",
    "finally",
    "for",
    "from",
    "function",
    "get",
    "if",
    "import",
    "in",
    "instanceof",
    "let",
    "new",
    "null",
    "of",
    "return",
    "set",
    "static",
    "super",
    "switch",
    "this",
    "throw",
    "true",
    "try",
    "typeof",
    "undefined",
    "var",
    "void",
    "while",
    "with",
    "yield",
];
const GO_KEYWORDS: &[&str] = &[
    "break",
    "case",
    "chan",
    "const",
    "continue",
    "default",
    "defer",
    "else",
    "fallthrough",
    "false",
    "for",
    "func",
    "go",
    "goto",
    "if",
    "import",
    "interface",
    "map",
    "nil",
    "package",
    "range",
    "return",
    "select",
    "struct",
    "switch",
    "true",
    "type",
    "var",
];
const C_KEYWORDS: &[&str] = &[
    "auto",
    "bool",
    "break",
    "case",
    "catch",
    "char",
    "class",
    "const",
    "continue",
    "default",
    "delete",
    "do",
    "double",
    "else",
    "enum",
    "extern",
    "false",
    "final",
    "float",
    "for",
    "goto",
    "if",
    "inline",
    "int",
    "long",
    "namespace",
    "new",
    "nullptr",
    "private",
    "protected",
    "public",
    "register",
    "return",
    "short",
    "signed",
    "sizeof",
    "static",
    "struct",
    "switch",
    "template",
    "this",
    "throw",
    "true",
    "try",
    "typedef",
    "typename",
    "union",
    "unsigned",
    "using",
    "virtual",
    "void",
    "volatile",
    "while",
];
const SHELL_KEYWORDS: &[&str] = &[
    "alias", "break", "case", "cd", "continue", "declare", "do", "done", "echo", "elif", "else",
    "esac", "export", "false", "fi", "for", "function", "if", "in", "local", "readonly", "return",
    "set", "shift", "source", "then", "trap", "true", "unset", "while",
];
const SQL_KEYWORDS: &[&str] = &[
    "all",
    "alter",
    "and",
    "as",
    "avg",
    "between",
    "by",
    "case",
    "count",
    "create",
    "delete",
    "distinct",
    "drop",
    "else",
    "end",
    "exists",
    "false",
    "foreign",
    "from",
    "full",
    "group",
    "having",
    "if",
    "in",
    "index",
    "inner",
    "insert",
    "into",
    "is",
    "join",
    "left",
    "like",
    "limit",
    "max",
    "min",
    "not",
    "null",
    "offset",
    "on",
    "or",
    "order",
    "outer",
    "primary",
    "references",
    "right",
    "select",
    "set",
    "sum",
    "table",
    "then",
    "true",
    "union",
    "unique",
    "update",
    "values",
    "view",
    "when",
    "where",
];
const JSON_KEYWORDS: &[&str] = &["false", "null", "true"];
const TOML_KEYWORDS: &[&str] = &["false", "true"];
const YAML_KEYWORDS: &[&str] = &["false", "no", "null", "off", "on", "true", "yes"];

const RUST_SPEC: CodeSpec = CodeSpec {
    line_comments: &["//"],
    block_comment: Some(("/*", "*/")),
    // `'` is excluded on purpose: Rust lifetimes would open a string that never closes.
    quotes: b"\"",
    triple_quotes: false,
    keywords: RUST_KEYWORDS,
};
const PYTHON_SPEC: CodeSpec = CodeSpec {
    line_comments: &["#"],
    block_comment: None,
    quotes: b"\"'",
    triple_quotes: true,
    keywords: PYTHON_KEYWORDS,
};
const JS_SPEC: CodeSpec = CodeSpec {
    line_comments: &["//"],
    block_comment: Some(("/*", "*/")),
    quotes: b"\"'`",
    triple_quotes: false,
    keywords: JS_KEYWORDS,
};
const GO_SPEC: CodeSpec = CodeSpec {
    line_comments: &["//"],
    block_comment: Some(("/*", "*/")),
    quotes: b"\"'",
    triple_quotes: false,
    keywords: GO_KEYWORDS,
};
const C_SPEC: CodeSpec = CodeSpec {
    line_comments: &["//"],
    block_comment: Some(("/*", "*/")),
    quotes: b"\"'",
    triple_quotes: false,
    keywords: C_KEYWORDS,
};
const SHELL_SPEC: CodeSpec = CodeSpec {
    line_comments: &["#"],
    block_comment: None,
    quotes: b"\"'",
    triple_quotes: false,
    keywords: SHELL_KEYWORDS,
};
const SQL_SPEC: CodeSpec = CodeSpec {
    line_comments: &["--"],
    block_comment: None,
    quotes: b"\"'",
    triple_quotes: false,
    keywords: SQL_KEYWORDS,
};
const JSON_SPEC: CodeSpec = CodeSpec {
    line_comments: &[],
    block_comment: None,
    quotes: b"\"",
    triple_quotes: false,
    keywords: JSON_KEYWORDS,
};
const TOML_SPEC: CodeSpec = CodeSpec {
    line_comments: &["#"],
    block_comment: None,
    quotes: b"\"'",
    triple_quotes: false,
    keywords: TOML_KEYWORDS,
};
const YAML_SPEC: CodeSpec = CodeSpec {
    line_comments: &["#"],
    block_comment: None,
    quotes: b"\"'",
    triple_quotes: false,
    keywords: YAML_KEYWORDS,
};

/// The spec for a fenced block's language tag, or `None` for anything unrecognised — which keeps
/// the old single-colour rendering rather than guessing at syntax.
fn language_spec(language: &str) -> Option<CodeSpec> {
    let lang = language.trim().to_ascii_lowercase();
    Some(match lang.as_str() {
        "rust" | "rs" => RUST_SPEC,
        "python" | "py" | "python3" => PYTHON_SPEC,
        "javascript" | "js" | "jsx" | "mjs" | "cjs" | "typescript" | "ts" | "tsx" => JS_SPEC,
        "go" | "golang" => GO_SPEC,
        "c" | "h" | "cpp" | "c++" | "cc" | "cxx" | "hpp" | "hh" | "java" | "cs" | "csharp"
        | "php" | "swift" | "kotlin" | "scala" | "dart" | "objc" | "objective-c" => C_SPEC,
        "sh" | "bash" | "zsh" | "shell" | "shellscript" | "console" => SHELL_SPEC,
        "sql" => SQL_SPEC,
        "json" => JSON_SPEC,
        "toml" => TOML_SPEC,
        "yaml" | "yml" => YAML_SPEC,
        _ => return None,
    })
}

/// Classify every byte of one code line, carrying block comments and triple-quoted strings
/// across lines through `state`. All token boundaries land on ASCII delimiters or the line end,
/// so the byte-indexed modes always slice at character boundaries.
fn highlight_modes(line: &str, spec: &CodeSpec, state: &mut CodeState) -> Vec<CodeMode> {
    let bytes = line.as_bytes();
    let n = bytes.len();
    let mut modes = vec![CodeMode::Code; n];
    let mut i = 0usize;

    while i < n {
        match *state {
            CodeState::BlockComment => {
                let end = spec.block_comment.map(|(_, end)| end).unwrap_or("*/");
                if bytes[i..].starts_with(end.as_bytes()) {
                    let stop = i + end.len();
                    modes[i..stop].fill(CodeMode::Comment);
                    i = stop;
                    *state = CodeState::Normal;
                } else {
                    modes[i..].fill(CodeMode::Comment);
                    break;
                }
            }
            CodeState::TripleString(quote) => {
                let close = [quote, quote, quote];
                let mut j = i;
                let mut closed = false;
                while j < n {
                    if bytes[j] == b'\\' {
                        j += 2;
                        continue;
                    }
                    if bytes[j..].starts_with(&close) {
                        closed = true;
                        break;
                    }
                    j += 1;
                }
                if closed {
                    let stop = j + 3;
                    modes[i..stop].fill(CodeMode::Str);
                    i = stop;
                    *state = CodeState::Normal;
                } else {
                    modes[i..].fill(CodeMode::Str);
                    break;
                }
            }
            CodeState::Normal => {
                if let Some((open, _)) = spec.block_comment
                    && bytes[i..].starts_with(open.as_bytes())
                {
                    modes[i..i + open.len()].fill(CodeMode::Comment);
                    i += open.len();
                    *state = CodeState::BlockComment;
                    continue;
                }
                if spec
                    .line_comments
                    .iter()
                    .any(|comment| bytes[i..].starts_with(comment.as_bytes()))
                {
                    modes[i..].fill(CodeMode::Comment);
                    break;
                }
                if spec.triple_quotes {
                    let mut opened = false;
                    for quote in spec.quotes {
                        let triple = [*quote, *quote, *quote];
                        if bytes[i..].starts_with(&triple) {
                            modes[i..i + 3].fill(CodeMode::Str);
                            i += 3;
                            *state = CodeState::TripleString(*quote);
                            opened = true;
                            break;
                        }
                    }
                    if opened {
                        continue;
                    }
                }
                if spec.quotes.contains(&bytes[i]) {
                    let quote = bytes[i];
                    let mut j = i + 1;
                    while j < n {
                        if bytes[j] == b'\\' {
                            j += 2;
                            continue;
                        }
                        if bytes[j] == quote {
                            j += 1;
                            break;
                        }
                        j += 1;
                    }
                    let stop = j.min(n);
                    modes[i..stop].fill(CodeMode::Str);
                    i = stop;
                    continue;
                }
                let len = keyword_len(bytes, i, spec.keywords);
                if len > 0 {
                    modes[i..i + len].fill(CodeMode::Keyword);
                    i += len;
                    continue;
                }
                i += 1;
            }
        }
    }
    modes
}

/// Length of the keyword starting at `i`, or 0. Both sides of the match must sit on identifier
/// boundaries, so `fn` inside `myfn` and `in` inside `begin` stay plain code.
fn keyword_len(bytes: &[u8], i: usize, keywords: &[&str]) -> usize {
    if i > 0 && is_ident_byte(bytes[i - 1]) {
        return 0;
    }
    for keyword in keywords {
        let word = keyword.as_bytes();
        if i + word.len() <= bytes.len()
            && &bytes[i..i + word.len()] == word
            && (i + word.len() == bytes.len() || !is_ident_byte(bytes[i + word.len()]))
        {
            return word.len();
        }
    }
    0
}

fn is_ident_byte(byte: u8) -> bool {
    byte.is_ascii_alphanumeric() || byte == b'_'
}

fn mode_style(mode: CodeMode, base: Style, theme: &Theme) -> Style {
    match mode {
        CodeMode::Code => base,
        CodeMode::Keyword => Style::default().fg(theme.accent),
        CodeMode::Str => Style::default().fg(theme.tool),
        CodeMode::Comment => theme.muted(),
    }
}

/// Merge a line's byte modes into styled pieces; adjacent equal styles collapse into one span.
fn code_pieces(
    line: &str,
    spec: &CodeSpec,
    state: &mut CodeState,
    base: Style,
    theme: &Theme,
) -> Vec<Piece> {
    let modes = highlight_modes(line, spec, state);
    let mut pieces = Vec::new();
    let mut start = 0usize;
    let mut current = modes.first().map(|mode| mode_style(*mode, base, theme));
    for (index, mode) in modes.iter().enumerate().skip(1) {
        let style = mode_style(*mode, base, theme);
        if Some(style) != current {
            if let Some(style) = current {
                pieces.push(Piece::new(line[start..index].to_owned(), style));
            }
            start = index;
            current = Some(style);
        }
    }
    if let Some(style) = current {
        pieces.push(Piece::new(line[start..].to_owned(), style));
    }
    pieces
}

fn table_rows(
    rows: &[Vec<String>],
    width: usize,
    theme: &Theme,
    base: Style,
) -> Vec<Line<'static>> {
    let columns = rows.iter().map(Vec::len).max().unwrap_or(0);
    if columns == 0 {
        return Vec::new();
    }
    let mut widths = vec![1usize; columns];
    for row in rows {
        for (column, cell) in row.iter().enumerate() {
            widths[column] = widths[column].max(cell.as_str().width());
        }
    }
    let gutter = if theme.ascii { "| " } else { "│ " };
    let divider = if theme.ascii { " | " } else { " │ " };

    let mut out: Vec<Line<'static>> = Vec::new();
    for (row_index, row) in rows.iter().enumerate() {
        let style = if row_index == 0 {
            base.add_modifier(Modifier::BOLD)
        } else {
            base
        };
        let mut pieces = vec![Piece::new(gutter, theme.muted())];
        for (column, w) in widths.iter().enumerate().take(columns) {
            let cell = row.get(column).map(String::as_str).unwrap_or("");
            pieces.push(Piece::new(cell.to_owned(), style));
            let pad = w.saturating_sub(cell.width());
            if pad > 0 {
                pieces.push(Piece::new(" ".repeat(pad), style));
            }
            if column + 1 < columns {
                pieces.push(Piece::new(divider, theme.muted()));
            }
        }
        out.extend(render(pieces, Vec::new(), Vec::new(), width, false));
    }
    out
}

fn fence_open(trimmed: &str) -> Option<(&'static str, &str)> {
    if let Some(rest) = trimmed.strip_prefix("```") {
        return Some(("```", rest.trim()));
    }
    if let Some(rest) = trimmed.strip_prefix("~~~") {
        return Some(("~~~", rest.trim()));
    }
    None
}

fn is_table_separator(line: &str) -> bool {
    let trimmed = line.trim();
    if trimmed.is_empty() || !trimmed.contains('-') || !trimmed.contains('|') {
        return false;
    }
    trimmed
        .chars()
        .all(|c| matches!(c, '-' | '|' | ':' | ' ' | '\t'))
}

fn split_row(line: &str) -> Vec<String> {
    let trimmed = line.trim();
    let trimmed = trimmed.strip_prefix('|').unwrap_or(trimmed);
    let trimmed = trimmed.strip_suffix('|').unwrap_or(trimmed);
    trimmed
        .split('|')
        .map(|cell| cell.trim().to_owned())
        .collect()
}

fn heading(trimmed: &str) -> Option<(usize, &str)> {
    let hashes = trimmed.chars().take_while(|c| *c == '#').count();
    if hashes == 0 || hashes > 6 {
        return None;
    }
    let rest = &trimmed[hashes..];
    if rest.is_empty() {
        return Some((hashes, ""));
    }
    if !rest.starts_with(' ') {
        return None;
    }
    Some((hashes, rest.trim()))
}

fn list_item(row: &str, theme: &Theme) -> Option<Vec<Piece>> {
    let indent_len = row.len() - row.trim_start().len();
    let indent = &row[..indent_len];
    let rest = &row[indent_len..];
    let bullet = if theme.ascii { "- " } else { "• " };

    for marker in ["- ", "* ", "+ "] {
        if let Some(content) = rest.strip_prefix(marker) {
            let mut pieces = vec![Piece::new(indent.to_owned(), Style::default())];
            pieces.push(Piece::new(bullet, Style::default().fg(theme.accent_dim)));
            pieces.extend(inline(content, theme.body(), theme));
            return Some(pieces);
        }
    }

    // Ordered items keep their number: the marker is part of the meaning.
    let digits = rest.chars().take_while(|c| c.is_ascii_digit()).count();
    if digits > 0 {
        let after = &rest[digits..];
        if let Some(content) = after
            .strip_prefix(". ")
            .or_else(|| after.strip_prefix(") "))
        {
            let marker_end = digits + 2;
            let mut pieces = vec![Piece::new(indent.to_owned(), Style::default())];
            pieces.push(Piece::new(rest[..marker_end].to_owned(), theme.secondary()));
            pieces.extend(inline(content, theme.body(), theme));
            return Some(pieces);
        }
    }
    None
}

// ---------------------------------------------------------------------------
// inline
// ---------------------------------------------------------------------------

fn inline(text: &str, base: Style, theme: &Theme) -> Vec<Piece> {
    let chars: Vec<char> = text.chars().collect();
    inline_chars(&chars, base, theme, 0)
}

fn inline_chars(chars: &[char], base: Style, theme: &Theme, depth: usize) -> Vec<Piece> {
    let mut out: Vec<Piece> = Vec::new();
    let mut buffer = String::new();
    let mut index = 0usize;

    while index < chars.len() {
        let current = chars[index];

        if current == '`'
            && let Some(end) = find_char(chars, index + 1, '`')
        {
            push_buffer(&mut out, &mut buffer, base);
            let code: String = chars[index + 1..end].iter().collect();
            if !code.is_empty() {
                out.push(Piece::new(code, base.fg(theme.tool)));
            }
            index = end + 1;
            continue;
        }

        if current == '*' && depth < MAX_INLINE_DEPTH {
            if index + 1 < chars.len() && chars[index + 1] == '*' {
                if let Some(end) = find_double_star(chars, index + 2)
                    && end > index + 2
                {
                    push_buffer(&mut out, &mut buffer, base);
                    out.extend(inline_chars(
                        &chars[index + 2..end],
                        base.add_modifier(Modifier::BOLD),
                        theme,
                        depth + 1,
                    ));
                    index = end + 2;
                    continue;
                }
            } else if let Some(end) = find_char(chars, index + 1, '*')
                && end > index + 1
            {
                push_buffer(&mut out, &mut buffer, base);
                out.extend(inline_chars(
                    &chars[index + 1..end],
                    base.add_modifier(Modifier::ITALIC),
                    theme,
                    depth + 1,
                ));
                index = end + 1;
                continue;
            }
        }

        buffer.push(current);
        index += 1;
    }

    push_buffer(&mut out, &mut buffer, base);
    out
}

fn find_char(chars: &[char], from: usize, target: char) -> Option<usize> {
    (from..chars.len()).find(|index| chars[*index] == target)
}

fn find_double_star(chars: &[char], from: usize) -> Option<usize> {
    if from >= chars.len() {
        return None;
    }
    (from..chars.len().saturating_sub(1))
        .find(|index| chars[*index] == '*' && chars[*index + 1] == '*')
}

fn push_buffer(out: &mut Vec<Piece>, buffer: &mut String, style: Style) {
    if !buffer.is_empty() {
        out.push(Piece::new(std::mem::take(buffer), style));
    }
}

// ---------------------------------------------------------------------------
// wrapping
// ---------------------------------------------------------------------------

fn plain(pieces: Vec<Piece>, width: usize) -> Vec<Line<'static>> {
    render(pieces, Vec::new(), Vec::new(), width, true)
}

/// Convert pieces to styled lines, hard-wrapping to `width` display columns.
///
/// `first_prefix` opens the first line, `cont_prefix` opens every continuation, which is how a
/// code block repeats its gutter. Wrapping never splits a character; when `break_on_space` is
/// set a line breaks at the last space that fits and that space is dropped.
fn render(
    pieces: Vec<Piece>,
    first_prefix: Vec<Piece>,
    cont_prefix: Vec<Piece>,
    width: usize,
    break_on_space: bool,
) -> Vec<Line<'static>> {
    let width = width.max(1);
    let chars = to_chars(&pieces);
    let mut prefix = first_prefix;

    if chars.is_empty() {
        return vec![build_line(&prefix, &[])];
    }

    let mut out: Vec<Line<'static>> = Vec::new();
    let mut position = 0usize;

    loop {
        let available = width.saturating_sub(prefix_width(&prefix)).max(1);
        let mut used = 0usize;
        let mut end = position;
        while end < chars.len() {
            let cell = cell_width(chars[end].0);
            if used + cell > available && end > position {
                break;
            }
            used += cell;
            end += 1;
        }

        if end >= chars.len() {
            out.push(build_line(&prefix, &chars[position..]));
            break;
        }

        let split = if break_on_space {
            (position + 1..end).rev().find(|at| chars[*at].0 == ' ')
        } else {
            None
        };

        let (line_end, mut next) = match split {
            Some(at) => (at, at + 1),
            None => (end, end),
        };
        out.push(build_line(&prefix, &chars[position..line_end]));
        if split.is_some() {
            while next < chars.len() && chars[next].0 == ' ' {
                next += 1;
            }
        }
        position = next;
        prefix = cont_prefix.clone();
    }

    out
}

fn to_chars(pieces: &[Piece]) -> Vec<(char, Style)> {
    let mut out: Vec<(char, Style)> = Vec::new();
    for piece in pieces {
        for ch in piece.text.chars() {
            out.push((ch, piece.style));
        }
    }
    out
}

fn build_line(prefix: &[Piece], chars: &[(char, Style)]) -> Line<'static> {
    let end = chars
        .iter()
        .rposition(|(ch, _)| *ch != ' ')
        .map(|index| index + 1)
        .unwrap_or(0);
    let chars = &chars[..end];

    let mut spans: Vec<Span<'static>> = Vec::with_capacity(prefix.len() + 1);
    for piece in prefix {
        if !piece.text.is_empty() {
            spans.push(Span::styled(piece.text.clone(), piece.style));
        }
    }

    let mut run = String::new();
    let mut run_style: Option<Style> = None;
    for (ch, style) in chars {
        if let Some(current) = run_style
            && current != *style
        {
            spans.push(Span::styled(std::mem::take(&mut run), current));
        }
        run_style = Some(*style);
        run.push(*ch);
    }
    if let Some(style) = run_style
        && !run.is_empty()
    {
        spans.push(Span::styled(run, style));
    }
    Line::from(spans)
}

fn prefix_width(prefix: &[Piece]) -> usize {
    prefix.iter().map(|piece| piece.text.as_str().width()).sum()
}

fn cell_width(ch: char) -> usize {
    ch.width().unwrap_or(0)
}

fn line_is_blank(line: &Line<'_>) -> bool {
    line.spans.iter().all(|span| span.content.trim().is_empty())
}

// ---------------------------------------------------------------------------
// ansi
// ---------------------------------------------------------------------------

/// Strip ANSI escape sequences (CSI and OSC forms) without a dependency.
fn strip_ansi(text: &str) -> String {
    let mut out = String::with_capacity(text.len());
    let mut chars = text.chars().peekable();
    while let Some(ch) = chars.next() {
        if ch != '\u{1b}' {
            out.push(ch);
            continue;
        }
        match chars.peek().copied() {
            // Control sequence: consume up to and including the final byte.
            Some('[') => {
                chars.next();
                for next in chars.by_ref() {
                    if ('\u{40}'..='\u{7e}').contains(&next) {
                        break;
                    }
                }
            }
            // Operating system command: terminated by BEL or ESC \.
            Some(']') => {
                chars.next();
                while let Some(next) = chars.next() {
                    if next == '\u{7}' {
                        break;
                    }
                    if next == '\u{1b}' {
                        if chars.peek() == Some(&'\\') {
                            chars.next();
                        }
                        break;
                    }
                }
            }
            // Any other two-byte escape: drop the escape and its selector.
            Some(_) => {
                chars.next();
            }
            None => {}
        }
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::theme::Theme;

    fn text_of(line: &Line<'_>) -> String {
        line.spans
            .iter()
            .map(|span| span.content.as_ref())
            .collect()
    }

    fn joined(lines: &[Line<'_>]) -> String {
        lines.iter().map(text_of).collect::<Vec<_>>().join("\n")
    }

    fn display_width(line: &Line<'_>) -> usize {
        line.spans
            .iter()
            .map(|span| span.content.as_ref().width())
            .sum()
    }

    #[test]
    fn inline_code_and_bold_markers_are_stripped() {
        let theme = Theme::new(false);
        let lines = markdown_lines("use `cargo` and **bold** text", 60, &theme);
        let body = joined(&lines);
        assert_eq!(body, "use cargo and bold text");
        assert!(!body.contains('`'));
        assert!(!body.contains('*'));
    }

    #[test]
    fn inline_styles_are_actually_applied() {
        let theme = Theme::new(false);
        let lines = markdown_lines("a `c` b", 60, &theme);
        let code = lines[0]
            .spans
            .iter()
            .find(|span| span.content.as_ref() == "c")
            .expect("code span missing");
        assert_eq!(code.style.fg, Some(theme.tool));

        let lines = markdown_lines("a **b**", 60, &theme);
        let bold = lines[0]
            .spans
            .iter()
            .find(|span| span.content.as_ref() == "b")
            .expect("bold span missing");
        assert!(bold.style.add_modifier.contains(Modifier::BOLD));
    }

    #[test]
    fn a_fenced_block_keeps_its_content_language_and_gutter() {
        let text = "before\n```rust\nfn main() {}\n```\nafter";
        let lines = markdown_lines(text, 40, &Theme::new(false));
        let body = joined(&lines);
        assert!(body.contains("rust"), "language label missing: {body}");
        assert!(body.contains("fn main() {}"), "code missing: {body}");
        assert!(body.contains('▏'), "unicode gutter missing: {body}");

        let lines = markdown_lines(text, 40, &Theme::new(true));
        let body = joined(&lines);
        assert!(body.contains('|'), "ascii gutter missing: {body}");
        assert!(body.contains("rust"));

        // An unknown language keeps the code uncoloured: one dim tone, no bleed from the
        // surroundings.
        let theme = Theme::new(false);
        let plain = markdown_lines("```text\nfn main() {}\n```", 40, &theme)
            .into_iter()
            .find(|line| text_of(line).contains("fn main"))
            .expect("code line missing");
        for span in &plain.spans {
            let fg = span.style.fg;
            assert!(
                fg == Some(theme.dim) || fg == Some(theme.faint),
                "code span is not dim: {fg:?}"
            );
        }
    }

    fn span_with<'a>(lines: &'a [Line<'a>], needle: &str) -> &'a Span<'a> {
        lines
            .iter()
            .flat_map(|line| line.spans.iter())
            .find(|span| span.content.as_ref() == needle)
            .unwrap_or_else(|| panic!("span {needle:?} missing"))
    }

    #[test]
    fn a_known_language_colours_keywords_strings_and_comments() {
        let theme = Theme::new(false);
        let lines = markdown_lines(
            "```rust\nfn main() {\n    // hi\n    let s = \"x\";\n}\n```",
            60,
            &theme,
        );
        assert_eq!(
            span_with(&lines, "fn").style.fg,
            Some(theme.accent),
            "keyword not accented"
        );
        assert_eq!(
            span_with(&lines, "\"x\"").style.fg,
            Some(theme.tool),
            "string not tool-coloured"
        );
        assert_eq!(
            span_with(&lines, "// hi").style.fg,
            Some(theme.faint),
            "comment not muted"
        );
        // The identifier between the tokens stays the dim code colour.
        let main = lines
            .iter()
            .flat_map(|line| line.spans.iter())
            .find(|span| span.content.contains("main"))
            .expect("identifier missing");
        assert_eq!(main.style.fg, Some(theme.dim));
    }

    #[test]
    fn keywords_respect_identifier_boundaries() {
        let theme = Theme::new(false);
        // `fn` inside `myfn` is not a keyword; `in` inside `begin` is not either.
        let lines = markdown_lines("```rust\nmyfn begin;\n```", 60, &theme);
        for line in &lines {
            for span in &line.spans {
                assert_ne!(
                    span.style.fg,
                    Some(theme.accent),
                    "identifier wrongly accented: {:?}",
                    span.content
                );
            }
        }
    }

    #[test]
    fn a_block_comment_and_triple_string_span_lines() {
        let theme = Theme::new(false);
        let rust = markdown_lines("```rust\n/* one\ntwo */\nfn f() {}\n```", 60, &theme);
        let comment = rust
            .iter()
            .find(|line| text_of(line).contains("two */"))
            .expect("comment line missing");
        let comment_span = comment
            .spans
            .iter()
            .find(|span| span.content.contains("two"))
            .expect("comment span missing");
        assert_eq!(comment_span.style.fg, Some(theme.faint));

        let python = markdown_lines(
            "```python\ns = \"\"\"one\ntwo\"\"\"\nreturn s\n```",
            60,
            &theme,
        );
        let middle = python
            .iter()
            .find(|line| text_of(line).contains("two"))
            .expect("string line missing");
        let string_span = middle
            .spans
            .iter()
            .find(|span| span.content.contains("two"))
            .expect("string span missing");
        assert_eq!(string_span.style.fg, Some(theme.tool));
        // The string closed: the next line's keyword is accented again.
        assert_eq!(span_with(&python, "return").style.fg, Some(theme.accent));
    }

    #[test]
    fn an_unclosed_fence_still_renders_its_body() {
        let theme = Theme::new(false);
        let lines = markdown_lines("```rust\nfn main() {}", 60, &theme);
        assert!(joined(&lines).contains("fn main() {}"));
        assert_eq!(span_with(&lines, "fn").style.fg, Some(theme.accent));
    }

    #[test]
    fn headings_strip_their_marker() {
        let theme = Theme::new(false);
        let h1 = markdown_lines("# Title", 40, &theme);
        assert_eq!(text_of(&h1[0]), "Title");
        assert_eq!(h1[0].spans[0].style.fg, Some(theme.accent));

        let h2 = markdown_lines("## Section", 40, &theme);
        assert_eq!(text_of(&h2[0]), "Section");
        assert_eq!(h2[0].spans[0].style.fg, Some(theme.accent));
    }

    #[test]
    fn lists_become_bullets_and_ordered_markers_stay() {
        let theme = Theme::new(false);
        let lines = markdown_lines("- one\n  - two\n1. first", 40, &theme);
        assert_eq!(text_of(&lines[0]), "• one");
        assert_eq!(text_of(&lines[1]), "  • two");
        assert_eq!(text_of(&lines[2]), "1. first");

        let ascii = markdown_lines("- one", 40, &Theme::new(true));
        assert_eq!(text_of(&ascii[0]), "- one");
    }

    #[test]
    fn blockquotes_get_a_dim_gutter() {
        let theme = Theme::new(false);
        let lines = markdown_lines("> quoted", 40, &theme);
        assert_eq!(text_of(&lines[0]), "│ quoted");
        let ascii = markdown_lines("> quoted", 40, &Theme::new(true));
        assert_eq!(text_of(&ascii[0]), "| quoted");
    }

    #[test]
    fn ascii_wrapping_never_exceeds_the_width() {
        let theme = Theme::new(false);
        let text = "the quick brown fox jumps over the lazy dog ".repeat(6);
        for width in [8, 12, 20, 33] {
            for line in markdown_lines(&text, width, &theme) {
                assert!(
                    display_width(&line) <= width,
                    "line wider than {width}: {:?}",
                    text_of(&line)
                );
            }
        }
    }

    #[test]
    fn cjk_wrapping_never_exceeds_the_width_and_never_splits_a_character() {
        let theme = Theme::new(false);
        let text = "漢字テストの表示幅".repeat(8);
        for width in [7, 10, 21, 40] {
            for line in markdown_lines(&text, width, &theme) {
                assert!(
                    display_width(&line) <= width,
                    "cjk line wider than {width}: {:?}",
                    text_of(&line)
                );
                for ch in text_of(&line).chars() {
                    assert!(
                        ch == ' ' || text.contains(ch),
                        "wrapping produced a broken character: {ch:?}"
                    );
                }
            }
        }
    }

    #[test]
    fn a_wrapped_code_block_repeats_its_gutter() {
        let theme = Theme::new(false);
        let text = "```\naaaaaaaaaaaaaaaaaaaaaaaaaaaaaa\n```";
        let lines = markdown_lines(text, 10, &theme);
        assert!(lines.len() > 2, "code line did not wrap");
        for line in &lines {
            assert!(
                text_of(line).starts_with('▏'),
                "gutter missing on {:?}",
                text_of(line)
            );
            assert!(display_width(line) <= 10);
        }
    }

    #[test]
    fn table_rows_are_padded_to_their_column_widths() {
        let theme = Theme::new(false);
        let lines = markdown_lines("| a | bb |\n|---|---|\n| ccc | d |", 80, &theme);
        assert_eq!(lines.len(), 2, "separator row should be consumed");
        let header = text_of(&lines[0]);
        let row = text_of(&lines[1]);
        assert!(header.contains('a') && header.contains("bb"), "{header}");
        assert!(row.contains("ccc") && row.contains('d'), "{row}");
        let bars = |text: &str| -> Vec<usize> {
            text.char_indices()
                .filter(|(_, ch)| *ch == '│')
                .map(|(index, _)| index)
                .collect()
        };
        assert_eq!(bars(&header), bars(&row), "columns are not aligned");
    }

    #[test]
    fn a_pipe_line_that_is_not_a_table_is_left_alone() {
        let theme = Theme::new(false);
        let lines = markdown_lines("a | b", 40, &theme);
        assert_eq!(text_of(&lines[0]), "a | b");
    }

    #[test]
    fn ansi_escapes_are_stripped() {
        let theme = Theme::new(false);
        let lines = markdown_lines(
            "\u{1b}[31mred\u{1b}[0m and \u{1b}[1mbold\u{1b}[0m",
            40,
            &theme,
        );
        assert_eq!(joined(&lines), "red and bold");
    }

    #[test]
    fn blank_lines_are_preserved_but_collapsed() {
        let theme = Theme::new(false);
        let lines = markdown_lines("a\n\n\n\nb", 10, &theme);
        assert_eq!(lines.len(), 3);
        assert_eq!(text_of(&lines[1]), "");
    }

    #[test]
    fn empty_input_renders_nothing() {
        let theme = Theme::new(false);
        assert!(markdown_lines("", 20, &theme).is_empty());
        assert!(markdown_lines("\n\n", 20, &theme).is_empty());
    }

    #[test]
    fn wrapping_is_deterministic_and_pure() {
        let theme = Theme::new(false);
        let text = "## Heading\n\nsome `code` and a [link](https://example.com)\n";
        let first = joined(&markdown_lines(text, 24, &theme));
        let second = joined(&markdown_lines(text, 24, &theme));
        assert_eq!(first, second);
    }
}
