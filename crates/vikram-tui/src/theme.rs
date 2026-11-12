//! Terminal palette and glyph set.
//!
//! Dark, minimal, orange accent. Every glyph that is not plain ASCII is exposed through the
//! `ascii` flag so a limited terminal degrades to a readable interface rather than boxes.

use ratatui::style::{Color, Modifier, Style};
use vikram_runtime::types::{BotState, NoticeLevel, RuntimeStatus, TurnState};

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Theme {
    pub ascii: bool,
    pub accent: Color,
    pub accent_dim: Color,
    pub text: Color,
    pub dim: Color,
    pub faint: Color,
    pub border: Color,
    pub bg: Color,
    pub surface: Color,
    /// Body of a dialog: a step above `bg`, so an open panel reads as a surface laid over the
    /// transcript instead of a line drawn around it.
    pub panel: Color,
    /// Band behind the row the cursor is on. Far enough from `bg` to be seen on a terminal that
    /// rounds colour, which `surface` is not.
    pub selection: Color,
    pub ok: Color,
    pub warn: Color,
    pub error: Color,
    pub user: Color,
    pub tool: Color,
}

impl Theme {
    pub fn new(ascii: bool) -> Self {
        Self {
            ascii,
            accent: Color::Rgb(255, 138, 0),
            accent_dim: Color::Rgb(170, 92, 0),
            text: Color::Rgb(225, 228, 232),
            dim: Color::Rgb(150, 156, 165),
            faint: Color::Rgb(92, 99, 110),
            border: Color::Rgb(62, 68, 78),
            bg: Color::Rgb(35, 37, 40),
            surface: Color::Rgb(41, 43, 47),
            panel: Color::Rgb(35, 37, 40),
            selection: Color::Rgb(58, 60, 68),
            ok: Color::Rgb(120, 200, 140),
            warn: Color::Rgb(230, 190, 100),
            error: Color::Rgb(235, 110, 110),
            user: Color::Rgb(200, 210, 225),
            tool: Color::Rgb(130, 170, 210),
        }
    }

    /// Overlay the operator's colours on the shipped palette. The window colour is also the body
    /// of a dialog, because a panel is separated from the transcript by its frame, not by a
    /// different fill — one value keeps the two from drifting apart. Everything else the operator
    /// did not name stays as shipped.
    pub fn with_colours(mut self, background: Option<Color>, accent: Option<Color>) -> Self {
        if let Some(background) = background {
            self.bg = background;
            self.panel = background;
        }
        if let Some(accent) = accent {
            self.accent = accent;
        }
        self
    }

    pub fn title(&self) -> Style {
        Style::default()
            .fg(self.accent)
            .add_modifier(Modifier::BOLD)
    }

    pub fn body(&self) -> Style {
        Style::default().fg(self.text)
    }

    pub fn secondary(&self) -> Style {
        Style::default().fg(self.dim)
    }

    pub fn muted(&self) -> Style {
        Style::default().fg(self.faint)
    }

    pub fn frame(&self) -> Style {
        Style::default().fg(self.border)
    }

    /// The body of a dialog. Every line a panel draws sits on this, so a stray line cannot punch a
    /// hole in the surface.
    pub fn dialog(&self) -> Style {
        Style::default().bg(self.panel)
    }

    /// The row the cursor is on. The band is the signal; colour alone is lost in a column of long
    /// names, and a terminal that quantises the palette can drop a subtle shade to nothing.
    pub fn selected(&self) -> Style {
        Style::default()
            .bg(self.selection)
            .fg(self.text)
            .add_modifier(Modifier::BOLD)
    }

    pub fn status_style(&self, segment: StatusSegment) -> Style {
        match segment {
            StatusSegment::Accent => Style::default().fg(self.accent),
            StatusSegment::Value => Style::default().fg(self.text),
            StatusSegment::Label => Style::default().fg(self.faint),
        }
    }

    pub fn notice_style(&self, level: NoticeLevel) -> Style {
        match level {
            NoticeLevel::Info => Style::default().fg(self.dim),
            NoticeLevel::Warn => Style::default().fg(self.warn),
            NoticeLevel::Error => Style::default().fg(self.error),
        }
    }

    pub fn agent_style(&self, state: BotState) -> Style {
        match state {
            BotState::Running | BotState::Thinking | BotState::Waiting => {
                Style::default().fg(self.accent)
            }
            BotState::Completed => Style::default().fg(self.ok),
            BotState::Failed => Style::default().fg(self.error),
            BotState::Paused => Style::default().fg(self.warn),
            _ => Style::default().fg(self.faint),
        }
    }

    pub fn runtime_style(&self, status: &RuntimeStatus) -> Style {
        match status {
            RuntimeStatus::Active => Style::default().fg(self.accent),
            RuntimeStatus::Idle => Style::default().fg(self.dim),
            RuntimeStatus::Initializing => Style::default().fg(self.warn),
            RuntimeStatus::Stopping => Style::default().fg(self.warn),
            RuntimeStatus::Error(_) => Style::default().fg(self.error),
        }
    }

    pub fn turn_style(&self, turn: TurnState) -> Style {
        match turn {
            TurnState::Idle => Style::default().fg(self.dim),
            TurnState::Streaming | TurnState::Tooling => Style::default().fg(self.accent),
            TurnState::WaitingApproval => Style::default().fg(self.warn),
            TurnState::Cancelled => Style::default().fg(self.warn),
            TurnState::Failed => Style::default().fg(self.error),
        }
    }

    // ---- glyphs ---------------------------------------------------------

    pub fn filled_dot(&self) -> &'static str {
        if self.ascii { "*" } else { "●" }
    }

    pub fn hollow_dot(&self) -> &'static str {
        if self.ascii { "o" } else { "○" }
    }

    pub fn prompt(&self) -> &'static str {
        if self.ascii { ">" } else { "❯" }
    }

    pub fn separator(&self) -> &'static str {
        if self.ascii { "-" } else { "─" }
    }

    pub fn bullet(&self) -> &'static str {
        if self.ascii { "*" } else { "·" }
    }

    pub fn model_glyph(&self) -> &'static str {
        if self.ascii { "@" } else { "☤" }
    }

    pub fn tasks_glyph(&self) -> &'static str {
        if self.ascii { "B" } else { "⏵" }
    }

    pub fn mcp_glyph(&self) -> &'static str {
        if self.ascii { "M" } else { "⌘" }
    }

    pub fn plugins_glyph(&self) -> &'static str {
        if self.ascii { "P" } else { "⧉" }
    }

    pub fn subagents_glyph(&self) -> &'static str {
        if self.ascii { "<>" } else { "◇" }
    }

    pub fn check(&self) -> &'static str {
        if self.ascii { "ok" } else { "✓" }
    }

    pub fn cross(&self) -> &'static str {
        if self.ascii { "!!" } else { "✕" }
    }

    pub fn dash(&self) -> &'static str {
        if self.ascii { "-" } else { "│" }
    }

    pub fn chevron(&self) -> &'static str {
        if self.ascii { ">" } else { "›" }
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum StatusSegment {
    Accent,
    Value,
    Label,
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn operator_colours_replace_the_window_and_the_accent_and_nothing_else() {
        let shipped = Theme::new(false);
        let themed =
            shipped.with_colours(Some(Color::Rgb(27, 28, 31)), Some(Color::Rgb(77, 163, 255)));
        assert_eq!(themed.bg, Color::Rgb(27, 28, 31));
        assert_eq!(themed.panel, Color::Rgb(27, 28, 31));
        assert_eq!(themed.accent, Color::Rgb(77, 163, 255));
        assert_eq!(themed.border, shipped.border);
        assert_eq!(themed.selection, shipped.selection);
        assert_eq!(themed.text, shipped.text);
    }

    #[test]
    fn ascii_mode_never_emits_wide_unicode_glyphs() {
        let theme = Theme::new(true);
        for glyph in [
            theme.filled_dot(),
            theme.hollow_dot(),
            theme.prompt(),
            theme.separator(),
            theme.model_glyph(),
            theme.subagents_glyph(),
            theme.dash(),
        ] {
            assert!(glyph.is_ascii(), "`{glyph}` is not ascii");
        }
    }

    #[test]
    fn unicode_mode_uses_the_approved_glyphs() {
        let theme = Theme::new(false);
        assert_eq!(theme.filled_dot(), "●");
        assert_eq!(theme.hollow_dot(), "○");
        assert_eq!(theme.subagents_glyph(), "◇");
        assert_eq!(theme.prompt(), "❯");
    }
}
