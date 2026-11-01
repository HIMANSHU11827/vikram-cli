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
    // Code is deliberately uncoloured: a dim, single-colour block cannot bleed into the body.
    let code_style = theme.secondary();

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
        out.extend(render(
            vec![Piece::new(line.clone(), code_style)],
            prefix.clone(),
            prefix.clone(),
            width,
            false,
        ));
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

        if current == '`' {
            if let Some(end) = find_char(chars, index + 1, '`') {
                push_buffer(&mut out, &mut buffer, base);
                let code: String = chars[index + 1..end].iter().collect();
                if !code.is_empty() {
                    out.push(Piece::new(code, base.fg(theme.tool)));
                }
                index = end + 1;
                continue;
            }
        }

        if current == '*' && depth < MAX_INLINE_DEPTH {
            if index + 1 < chars.len() && chars[index + 1] == '*' {
                if let Some(end) = find_double_star(chars, index + 2) {
                    if end > index + 2 {
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
                }
            } else if let Some(end) = find_char(chars, index + 1, '*') {
                if end > index + 1 {
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
        if let Some(current) = run_style {
            if current != *style {
                spans.push(Span::styled(std::mem::take(&mut run), current));
            }
        }
        run_style = Some(*style);
        run.push(*ch);
    }
    if let Some(style) = run_style {
        if !run.is_empty() {
            spans.push(Span::styled(run, style));
        }
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

        // The code line itself is uncoloured: one dim tone, no bleed from the surroundings.
        let theme = Theme::new(false);
        let code_line = markdown_lines(text, 40, &theme)
            .into_iter()
            .find(|line| text_of(line).contains("fn main"))
            .expect("code line missing");
        for span in &code_line.spans {
            let fg = span.style.fg;
            assert!(
                fg == Some(theme.dim) || fg == Some(theme.faint),
                "code span is not dim: {fg:?}"
            );
        }
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
