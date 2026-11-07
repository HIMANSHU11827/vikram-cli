//! The transcript's one indent rule.
//!
//! Each row renderer used to pick its own prefix: a tool call sat flush with the panel, an activity
//! carried its fold mark in front of its glyph, and a speech body started a column further in than
//! either. Rows that are the same event in different shapes then read as different nesting levels.
//! One rule keeps them in one column:
//!
//! - a row inside a turn sits one [`STEP`] right of the name row that opened the turn;
//! - a row's body — a wrapped continuation, a payload, or a nested section — starts under the text
//!   its own glyph introduces, which is one [`STEP`] past that text for a one-column glyph.
//!
//! Every renderer asks this module for its columns instead of spelling a prefix of its own, so a
//! change to the rule lands in all of them at once.

use unicode_width::UnicodeWidthStr;

/// The width, in columns, of one nesting step.
pub const STEP: usize = 2;

/// The depth of a row that belongs to a turn.
///
/// A turn is named at depth zero — `> VIKRAM`, `❯ User`, `· system` — and everything it did hangs
/// one step under that name.
pub const TURN: usize = 1;

/// The column a row at `depth` starts at.
pub fn row(depth: usize) -> usize {
    depth * STEP
}

/// The column a row's body starts at, given the glyph the row opens with.
///
/// The body belongs under the text the glyph introduces, so a wrapped line continues in the column
/// of the line it continues, and a two-column ascii glyph widens both by the same amount.
pub fn body(depth: usize, glyph: &str) -> usize {
    row(depth) + glyph.width() + 1
}

/// The column a block nested inside a row at `depth` starts at: exactly one [`STEP`] further in.
///
/// A payload behind a tool row is one of these: it is not a continuation of the row's sentence but
/// a body of its own, so it steps in rather than lining up with the row's text.
pub fn nested(depth: usize) -> usize {
    row(depth + 1)
}

/// [`row`] as the run of spaces a line starts with.
pub fn row_prefix(depth: usize) -> String {
    spaces(row(depth))
}

/// [`body`] as the run of spaces a line starts with.
pub fn body_prefix(depth: usize, glyph: &str) -> String {
    spaces(body(depth, glyph))
}

/// A run of `columns` spaces, for a line that starts at a column the rules above decided.
pub fn spaces(columns: usize) -> String {
    " ".repeat(columns)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_row_inside_a_turn_is_one_step_in_from_the_name_row() {
        assert_eq!(row(0), 0, "a turn's own name row is flush");
        assert_eq!(row(TURN), STEP, "everything the turn did steps in once");
        assert_eq!(row(2), 2 * STEP, "and a row under that steps in again");
    }

    #[test]
    fn a_body_starts_under_the_text_its_glyph_introduces() {
        // Every unicode glyph the transcript uses is one column wide, so the text of a row is one
        // step past the row.
        assert_eq!(body(TURN, "✓"), row(TURN) + 2);
        assert_eq!(body(TURN, "✓"), row(2));
        // The ascii fallbacks are two columns wide and carry their text one further still.
        assert_eq!(body(TURN, "ok"), row(TURN) + 3);
        assert_eq!(body(TURN, "→"), body(TURN, "✓"));
        assert_eq!(body(TURN, "->"), body(TURN, "ok"));
    }

    #[test]
    fn a_nested_block_steps_in_by_exactly_one_step() {
        for depth in 0..4 {
            assert_eq!(nested(depth), row(depth) + STEP);
            assert_eq!(nested(depth), row(depth + 1));
        }
    }

    #[test]
    fn prefixes_are_the_columns_they_name() {
        assert_eq!(row_prefix(TURN), spaces(2));
        assert_eq!(body_prefix(TURN, "✓"), spaces(4));
        assert_eq!(body_prefix(TURN, "ok"), spaces(5));
        assert_eq!(spaces(0), "");
        for columns in 0..8 {
            assert_eq!(spaces(columns).len(), columns);
        }
    }
}
