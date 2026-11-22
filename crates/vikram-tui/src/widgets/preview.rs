//! What approving a call will actually change.
//!
//! The runtime attaches an [`ApprovalPreview`] when a tool can describe its effect. This module
//! turns one into rows: the TUI draws them under the question, and the CLI prints the same text,
//! so both interfaces say the same thing about the same change.

use ratatui::style::{Modifier, Style};
use ratatui::text::{Line, Span};
use vikram_runtime::types::ApprovalPreview;

use crate::theme::Theme;

/// Rows a preview may take. The runtime caps each side at 200 lines and says how many it cut; this
/// is the interface's own budget, so one large change cannot push the question off the screen.
pub const PREVIEW_LINES: usize = 24;

/// Column a long command wraps at.
const WRAP: usize = 72;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Kind {
    Path,
    Minus,
    Plus,
    Cwd,
    Command,
    Title,
    Text,
    Note,
}

/// One row of a preview, before it is given a style: what it is, and what it says.
type Row = (Kind, String);

/// The preview's rows, styled for the panel it is drawn in.
pub fn preview_lines(preview: &ApprovalPreview, theme: &Theme, cap: usize) -> Vec<Line<'static>> {
    rows(preview, theme, cap)
        .into_iter()
        .map(|(kind, text)| Line::from(Span::styled(text, style_for(kind, theme))))
        .collect()
}

/// The same rows as plain text, for a caller that prints instead of drawing. Nothing here is
/// coloured: a log line has no palette. The ascii palette is used so the glyphs are the ones a
/// plain terminal can print.
pub fn preview_text(preview: &ApprovalPreview, cap: usize) -> Vec<String> {
    preview_lines(preview, &Theme::new(true), cap)
        .into_iter()
        .map(|line| {
            line.spans
                .iter()
                .map(|span| span.content.as_ref())
                .collect()
        })
        .collect()
}

fn style_for(kind: Kind, theme: &Theme) -> Style {
    match kind {
        Kind::Path => Style::default().fg(theme.tool),
        Kind::Minus => side_style(theme, theme.error),
        Kind::Plus => side_style(theme, theme.ok),
        Kind::Cwd => theme.muted(),
        Kind::Command => theme.body(),
        Kind::Title => Style::default().fg(theme.text).add_modifier(Modifier::BOLD),
        Kind::Text => theme.secondary(),
        Kind::Note => theme.muted(),
    }
}

/// A removed or added line is marked by `-`/`+` first and coloured second: ascii mode has no colour
/// to spend, and the marker already carries the meaning.
fn side_style(theme: &Theme, colour: ratatui::style::Color) -> Style {
    if theme.ascii {
        Style::default()
    } else {
        Style::default().fg(colour)
    }
}

fn rows(preview: &ApprovalPreview, theme: &Theme, cap: usize) -> Vec<Row> {
    let budget = cap.max(2);
    match preview {
        ApprovalPreview::Diff {
            path,
            before,
            after,
        } => diff_rows(path, before, after, budget, theme.ascii),
        ApprovalPreview::Command { program, args, cwd } => {
            command_rows(program, args, cwd, budget, theme.ascii)
        }
        ApprovalPreview::Body { title, text } => body_rows(title, text, budget, theme.ascii),
    }
}

/// The runtime shortens a long side itself and ends it with a note saying how much it cut. The note
/// is quoted verbatim rather than recomputed, and it is never the row a cap throws away: it is the
/// only thing that tells the operator the preview is partial.
fn split_note(lines: Vec<&str>) -> (Option<String>, Vec<&str>) {
    let mut kept = lines;
    let note = kept
        .last()
        .filter(|line| line.starts_with('…') && line.contains(" lines shown"))
        .map(|line| (*line).to_owned());
    if note.is_some() {
        kept.pop();
    }
    (note, kept)
}

/// Divide the row budget between the two sides, in proportion to their size. A side that exists
/// always keeps at least one row: an all-`-` preview hides the change the operator is judging.
fn share(before: usize, after: usize, total: usize) -> (usize, usize) {
    if total == 0 || before + after == 0 {
        return (0, 0);
    }
    let mut kept_before = total * before / (before + after);
    if before > 0 {
        kept_before = kept_before.max(1);
    }
    if after > 0 {
        kept_before = kept_before.min(total.saturating_sub(1));
    }
    let kept_before = kept_before.min(before).min(total);
    let kept_after = (total - kept_before).min(after);
    ((total - kept_after).min(before), kept_after)
}

fn diff_rows(path: &str, before: &str, after: &str, budget: usize, ascii: bool) -> Vec<Row> {
    let (before_note, before) = split_note(before.lines().collect());
    let (after_note, after) = split_note(after.lines().collect());
    let notes = usize::from(before_note.is_some()) + usize::from(after_note.is_some());
    let (room_before, room_after) =
        share(before.len(), after.len(), budget.saturating_sub(1 + notes));
    let mut rows = vec![(Kind::Path, path.to_owned())];
    rows.extend(side_rows(
        Kind::Minus,
        &before,
        before_note,
        room_before,
        ascii,
    ));
    rows.extend(side_rows(Kind::Plus, &after, after_note, room_after, ascii));
    rows
}

/// One side of a diff: its kept lines, one elision row when lines were dropped, then the runtime's
/// note. Never more rows than `room`, so the caller's budget is the whole preview's budget.
fn side_rows(
    kind: Kind,
    lines: &[&str],
    note: Option<String>,
    room: usize,
    ascii: bool,
) -> Vec<Row> {
    let marker = match kind {
        Kind::Plus => '+',
        _ => '-',
    };
    let mut rows = Vec::new();
    let content = room.saturating_sub(usize::from(note.is_some()));
    if lines.len() <= content {
        rows.extend(lines.iter().map(|line| (kind, format!("{marker}{line}"))));
    } else {
        let keep = content.saturating_sub(1);
        rows.extend(
            lines[..keep]
                .iter()
                .map(|line| (kind, format!("{marker}{line}"))),
        );
        rows.push((
            Kind::Note,
            format!("{marker}{}", elision(lines.len() - keep, ascii)),
        ));
    }
    if let Some(note) = note {
        rows.push((Kind::Note, note));
    }
    rows
}

fn elision(count: usize, ascii: bool) -> String {
    let marker = if ascii { '>' } else { '…' };
    format!("{marker} {count} more lines")
}

fn command_rows(program: &str, args: &[String], cwd: &str, budget: usize, ascii: bool) -> Vec<Row> {
    let mut rows = vec![(Kind::Cwd, format!("$ {cwd}"))];
    let mut command = program.to_owned();
    for arg in args.iter().filter(|arg| !arg.is_empty()) {
        command.push(' ');
        command.push_str(arg);
    }
    let wrapped = wrap(&command, WRAP);
    let room = budget.saturating_sub(1);
    for (index, line) in wrapped.iter().take(room).enumerate() {
        // A wrapped command reads as one command: the continuation is indented under the first
        // word rather than starting a new one at the margin.
        let text = if index == 0 {
            line.clone()
        } else {
            format!("  {line}")
        };
        rows.push((Kind::Command, text));
    }
    if wrapped.len() > room {
        rows.push((Kind::Note, elision(wrapped.len() - room, ascii)));
    }
    rows
}

fn body_rows(title: &str, text: &str, budget: usize, ascii: bool) -> Vec<Row> {
    let mut rows = vec![(Kind::Title, title.to_owned())];
    let lines: Vec<&str> = text.lines().collect();
    let room = budget.saturating_sub(1);
    rows.extend(
        lines
            .iter()
            .take(room)
            .map(|line| (Kind::Text, (*line).to_owned())),
    );
    if lines.len() > room {
        rows.push((Kind::Note, elision(lines.len() - room, ascii)));
    }
    rows
}

/// Break a command at spaces so it stays inside the panel. No word is split: a wrapped path is
/// still a path.
fn wrap(text: &str, width: usize) -> Vec<String> {
    use unicode_width::UnicodeWidthStr;

    let width = width.max(8);
    let mut out: Vec<String> = Vec::new();
    let mut current = String::new();
    for word in text.split_whitespace() {
        let separator = usize::from(!current.is_empty());
        if !current.is_empty() && current.width() + separator + word.width() > width {
            out.push(std::mem::take(&mut current));
        }
        if !current.is_empty() {
            current.push(' ');
        }
        current.push_str(word);
    }
    if !current.is_empty() {
        out.push(current);
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    fn texts(lines: &[Line<'static>]) -> Vec<String> {
        lines
            .iter()
            .map(|line| {
                line.spans
                    .iter()
                    .map(|span| span.content.as_ref())
                    .collect()
            })
            .collect()
    }

    fn capped_side(label: &str) -> String {
        let mut side: String = (0..200).map(|line| format!("{label} {line}\n")).collect();
        side.push_str("… [200 of 300 lines shown, 100 cut]");
        side
    }

    #[test]
    fn a_diff_renders_the_path_and_both_sides() {
        let preview = ApprovalPreview::Diff {
            path: "src/main.rs".to_owned(),
            before: "let a = 1;\n".to_owned(),
            after: "let a = 2;\n".to_owned(),
        };
        let theme = Theme::new(false);
        let lines = preview_lines(&preview, &theme, PREVIEW_LINES);
        assert_eq!(
            texts(&lines),
            vec!["src/main.rs", "-let a = 1;", "+let a = 2;"]
        );
        // The two sides are told apart by colour as well as by their marker, using the palette's
        // own error and ok roles rather than a colour invented for the diff.
        assert_eq!(lines[1].spans[0].style.fg, Some(theme.error));
        assert_eq!(lines[2].spans[0].style.fg, Some(theme.ok));
    }

    #[test]
    fn a_command_renders_its_directory_and_its_arguments() {
        let preview = ApprovalPreview::Command {
            program: "cargo".to_owned(),
            args: vec!["test".to_owned(), "-p".to_owned(), "vikram-tui".to_owned()],
            cwd: "/work".to_owned(),
        };
        let rendered = texts(&preview_lines(&preview, &Theme::new(false), PREVIEW_LINES));
        assert_eq!(rendered, vec!["$ /work", "cargo test -p vikram-tui"]);
    }

    #[test]
    fn a_long_command_wraps_instead_of_running_off_the_panel() {
        let preview = ApprovalPreview::Command {
            program: "cargo".to_owned(),
            args: (0..20).map(|index| format!("--flag-{index}")).collect(),
            cwd: "/work".to_owned(),
        };
        let rendered = texts(&preview_lines(&preview, &Theme::new(false), PREVIEW_LINES));
        assert!(rendered.len() > 2, "a long command must wrap: {rendered:?}");
        for line in &rendered {
            assert!(line.chars().count() <= WRAP, "line over budget: {line}");
        }
    }

    #[test]
    fn a_body_renders_its_title_then_its_text() {
        let preview = ApprovalPreview::Body {
            title: "message to send".to_owned(),
            text: "hello\nthere".to_owned(),
        };
        let theme = Theme::new(false);
        let lines = preview_lines(&preview, &theme, PREVIEW_LINES);
        assert_eq!(texts(&lines), vec!["message to send", "hello", "there"]);
        assert!(
            lines[0].spans[0]
                .style
                .add_modifier
                .contains(Modifier::BOLD),
            "the title is the one row that has to stand out"
        );
    }

    #[test]
    fn a_long_diff_is_capped_and_quotes_the_runtime_note() {
        let preview = ApprovalPreview::Diff {
            path: "big.txt".to_owned(),
            before: capped_side("old"),
            after: capped_side("new"),
        };
        let rendered = texts(&preview_lines(&preview, &Theme::new(false), PREVIEW_LINES));
        assert!(
            rendered.len() <= PREVIEW_LINES,
            "{} rows is over the {PREVIEW_LINES} row budget: {rendered:#?}",
            rendered.len()
        );
        // The runtime's own note is quoted, not paraphrased: it is the only thing that says how
        // much of the file the operator is not seeing.
        assert!(
            rendered
                .iter()
                .any(|line| line == "… [200 of 300 lines shown, 100 cut]"),
            "runtime note missing: {rendered:#?}"
        );
        assert!(rendered.iter().any(|line| line.starts_with('-')));
        assert!(rendered.iter().any(|line| line.starts_with('+')));
    }

    #[test]
    fn a_cap_too_small_for_content_still_says_the_side_was_cut() {
        let preview = ApprovalPreview::Diff {
            path: "big.txt".to_owned(),
            before: capped_side("old"),
            after: String::new(),
        };
        let rendered = texts(&preview_lines(&preview, &Theme::new(false), 4));
        assert!(rendered.len() <= 4, "{rendered:#?}");
        assert!(rendered[0] == "big.txt");
    }

    #[test]
    fn ascii_previews_carry_no_colour() {
        let preview = ApprovalPreview::Diff {
            path: "a.txt".to_owned(),
            before: "old\n".to_owned(),
            after: "new\n".to_owned(),
        };
        let theme = Theme::new(true);
        let lines = preview_lines(&preview, &theme, PREVIEW_LINES);
        // The diff rows are the ones a palette would colour; on an ascii terminal the marker has to
        // carry the whole meaning.
        for line in &lines[1..] {
            for span in &line.spans {
                assert_eq!(span.style.fg, None, "ascii mode must not colour {span:?}");
                assert_eq!(span.style.bg, None, "ascii mode must not fill {span:?}");
            }
        }
        for line in &lines {
            for span in &line.spans {
                assert!(
                    !span.content.contains('\u{1b}'),
                    "an escape sequence reached the buffer: {span:?}"
                );
            }
        }
        assert_eq!(texts(&lines), vec!["a.txt", "-old", "+new"]);
    }

    #[test]
    fn plain_text_matches_what_the_panel_draws() {
        let preview = ApprovalPreview::Command {
            program: "rm".to_owned(),
            args: vec!["-rf".to_owned(), "target".to_owned()],
            cwd: "/work".to_owned(),
        };
        assert_eq!(
            preview_text(&preview, PREVIEW_LINES),
            vec!["$ /work", "rm -rf target"]
        );
    }
}
