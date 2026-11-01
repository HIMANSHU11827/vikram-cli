//! A small multiline editor.
//!
//! Deliberately owned rather than delegated to a widget crate: the terminal is the only input
//! surface, so its semantics (submit vs newline, cursor movement, paste, history) need to be
//! deterministic and testable without a real terminal.

use unicode_width::UnicodeWidthChar;

#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct Editor {
    lines: Vec<String>,
    row: usize,
    col: usize,
    history: Vec<String>,
    history_cursor: Option<usize>,
    /// Saved current draft when the user starts browsing history.
    stash: Option<String>,
}

impl Editor {
    pub fn new() -> Self {
        Self {
            lines: vec![String::new()],
            ..Default::default()
        }
    }

    pub fn from_text(text: &str) -> Self {
        let mut editor = Self::new();
        editor.set_text(text);
        editor
    }

    pub fn text(&self) -> String {
        self.lines.join("\n")
    }

    pub fn is_empty(&self) -> bool {
        self.lines.len() == 1 && self.lines[0].is_empty()
    }

    pub fn line_count(&self) -> usize {
        self.lines.len()
    }

    pub fn line(&self, index: usize) -> Option<&str> {
        self.lines.get(index).map(String::as_str)
    }

    pub fn cursor(&self) -> (usize, usize) {
        (self.row, self.col)
    }

    pub fn set_text(&mut self, text: &str) {
        let text = text.replace("\r\n", "\n").replace('\r', "\n");
        self.lines = text.split('\n').map(str::to_owned).collect();
        if self.lines.is_empty() {
            self.lines.push(String::new());
        }
        self.row = self.lines.len() - 1;
        self.col = self.current().chars().count();
    }

    pub fn clear(&mut self) {
        self.lines = vec![String::new()];
        self.row = 0;
        self.col = 0;
        self.history_cursor = None;
        self.stash = None;
    }

    fn current(&self) -> &String {
        &self.lines[self.row]
    }

    fn current_mut(&mut self) -> &mut String {
        &mut self.lines[self.row]
    }

    fn char_index_to_byte(s: &str, char_index: usize) -> usize {
        s.char_indices()
            .nth(char_index)
            .map(|(i, _)| i)
            .unwrap_or(s.len())
    }

    pub fn insert_char(&mut self, ch: char) {
        if ch == '\n' {
            self.insert_newline();
            return;
        }
        let idx = Self::char_index_to_byte(self.current(), self.col);
        self.current_mut().insert(idx, ch);
        self.col += 1;
        self.history_cursor = None;
    }

    pub fn insert_str(&mut self, text: &str) {
        let normalized = text.replace("\r\n", "\n").replace('\r', "\n");
        for ch in normalized.chars() {
            self.insert_char(ch);
        }
    }

    pub fn insert_newline(&mut self) {
        let idx = Self::char_index_to_byte(self.current(), self.col);
        let tail = self.current()[idx..].to_owned();
        self.current_mut().truncate(idx);
        self.lines.insert(self.row + 1, tail);
        self.row += 1;
        self.col = 0;
        self.history_cursor = None;
    }

    pub fn backspace(&mut self) {
        if self.col > 0 {
            let idx = Self::char_index_to_byte(self.current(), self.col - 1);
            self.current_mut().remove(idx);
            self.col -= 1;
        } else if self.row > 0 {
            let tail = self.lines.remove(self.row);
            self.row -= 1;
            self.col = self.current().chars().count();
            self.current_mut().push_str(&tail);
        }
        self.history_cursor = None;
    }

    pub fn delete(&mut self) {
        let len = self.current().chars().count();
        if self.col < len {
            let idx = Self::char_index_to_byte(self.current(), self.col);
            self.current_mut().remove(idx);
        } else if self.row + 1 < self.lines.len() {
            let tail = self.lines.remove(self.row + 1);
            self.current_mut().push_str(&tail);
        }
        self.history_cursor = None;
    }

    pub fn move_left(&mut self) {
        if self.col > 0 {
            self.col -= 1;
        } else if self.row > 0 {
            self.row -= 1;
            self.col = self.current().chars().count();
        }
    }

    pub fn move_right(&mut self) {
        let len = self.current().chars().count();
        if self.col < len {
            self.col += 1;
        } else if self.row + 1 < self.lines.len() {
            self.row += 1;
            self.col = 0;
        }
    }

    pub fn move_up(&mut self) {
        if self.row > 0 {
            self.row -= 1;
            self.col = self.col.min(self.current().chars().count());
        }
    }

    pub fn move_down(&mut self) {
        if self.row + 1 < self.lines.len() {
            self.row += 1;
            self.col = self.col.min(self.current().chars().count());
        }
    }

    pub fn home(&mut self) {
        self.col = 0;
    }

    pub fn end(&mut self) {
        self.col = self.current().chars().count();
    }

    pub fn word_left(&mut self) {
        let chars: Vec<char> = self.current().chars().collect();
        let mut i = self.col;
        while i > 0 && chars[i - 1].is_whitespace() {
            i -= 1;
        }
        while i > 0 && !chars[i - 1].is_whitespace() {
            i -= 1;
        }
        self.col = i;
    }

    pub fn word_right(&mut self) {
        let chars: Vec<char> = self.current().chars().collect();
        let mut i = self.col;
        while i < chars.len() && chars[i].is_whitespace() {
            i += 1;
        }
        while i < chars.len() && !chars[i].is_whitespace() {
            i += 1;
        }
        self.col = i;
    }

    /// Delete from the cursor back to the start of the previous word.
    pub fn delete_word_left(&mut self) {
        if self.col == 0 {
            self.backspace();
            return;
        }
        let chars: Vec<char> = self.current().chars().collect();
        let mut start = self.col;
        while start > 0 && chars[start - 1].is_whitespace() {
            start -= 1;
        }
        while start > 0 && !chars[start - 1].is_whitespace() {
            start -= 1;
        }
        self.col = start;
        self.delete();
    }

    /// Remember a submitted message for Up/Down browsing.
    pub fn remember(&mut self, text: &str) {
        let text = text.trim();
        if text.is_empty() {
            return;
        }
        if self.history.last().map(String::as_str) != Some(text) {
            self.history.push(text.to_owned());
            if self.history.len() > 200 {
                self.history.remove(0);
            }
        }
        self.history_cursor = None;
        self.stash = None;
    }

    pub fn history_previous(&mut self) {
        if self.history.is_empty() {
            return;
        }
        let next = match self.history_cursor {
            None => {
                self.stash = Some(self.text());
                self.history.len() - 1
            }
            Some(0) => 0,
            Some(i) => i - 1,
        };
        self.history_cursor = Some(next);
        let entry = self.history[next].clone();
        self.set_text(&entry);
    }

    pub fn history_next(&mut self) {
        let Some(i) = self.history_cursor else {
            return;
        };
        if i + 1 < self.history.len() {
            self.history_cursor = Some(i + 1);
            let entry = self.history[i + 1].clone();
            self.set_text(&entry);
        } else {
            self.history_cursor = None;
            let stash = self.stash.take().unwrap_or_default();
            self.set_text(&stash);
        }
    }

    /// Visual width of the text, used to size the input box.
    /// Recorded submissions, oldest first. Used to suggest recent `!` commands.
    pub fn history(&self) -> &[String] {
        &self.history
    }

    pub fn max_line_width(&self) -> usize {
        self.lines
            .iter()
            .map(|l| l.chars().filter_map(UnicodeWidthChar::width).sum::<usize>())
            .max()
            .unwrap_or(0)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn typing_and_newlines_round_trip() {
        let mut e = Editor::new();
        e.insert_str("hello");
        e.insert_newline();
        e.insert_str("world");
        assert_eq!(e.text(), "hello\nworld");
        assert_eq!(e.line_count(), 2);
        assert_eq!(e.cursor(), (1, 5));
    }

    #[test]
    fn backspace_joins_lines_at_a_boundary() {
        let mut e = Editor::from_text("ab\ncd");
        e.move_up();
        e.end();
        e.delete();
        assert_eq!(e.text(), "abcd");
    }

    #[test]
    fn vertical_movement_clamps_to_the_shorter_line() {
        let mut e = Editor::from_text("longer line\nx");
        e.move_up();
        e.end();
        e.move_down();
        assert_eq!(e.cursor(), (1, 1));
    }

    #[test]
    fn paste_of_multiline_text_preserves_structure() {
        let mut e = Editor::new();
        e.insert_str("a\r\nb\rc");
        assert_eq!(e.text(), "a\nb\nc");
    }

    #[test]
    fn history_walks_backwards_and_restores_the_draft() {
        let mut e = Editor::new();
        e.remember("first");
        e.remember("second");
        e.insert_str("draft");
        e.history_previous();
        assert_eq!(e.text(), "second");
        e.history_previous();
        assert_eq!(e.text(), "first");
        e.history_next();
        e.history_next();
        assert_eq!(e.text(), "draft");
    }

    #[test]
    fn unicode_cursor_movement_is_char_based_not_byte_based() {
        let mut e = Editor::from_text("héllo→");
        assert_eq!(e.cursor(), (0, 6));
        e.word_left();
        assert_eq!(e.cursor(), (0, 0));
    }
}
