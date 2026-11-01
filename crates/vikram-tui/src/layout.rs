//! Responsive geometry.
//!
//! The four approved regions map onto the terminal's actual size. The input grows with its
//! content, the BOTS panel collapses before it can squeeze the conversation, and very short
//! terminals drop decoration rather than rows of content.

use ratatui::layout::Rect;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Regions {
    pub tabs: Rect,
    pub main: Rect,
    pub conversation: Rect,
    /// `None` when the panel is collapsed or no teams.
    pub hive: Option<Rect>,
    /// `None` when the panel is collapsed or no bots.
    pub bots: Option<Rect>,
    pub input: Rect,
    pub status: Rect,
    pub shortcuts: Option<Rect>,
    pub compact: bool,
}

pub const BOTS_MIN_WIDTH: u16 = 88;
pub const BOTS_COLLAPSE_HEIGHT: u16 = 16;

pub fn bots_width(total: u16) -> u16 {
    (total / 3).clamp(24, 40)
}

pub fn layout(area: Rect, show_hive: bool, show_bots: bool, input_lines: usize) -> Regions {
    let compact = area.height < BOTS_COLLAPSE_HEIGHT;
    let input_height = ((input_lines.max(1) as u16) + 2).clamp(3, 8);
    let shortcuts_height: u16 = if compact { 0 } else { 1 };
    let fixed = 1 + input_height + 1 + shortcuts_height;

    let main_height = area.height.saturating_sub(fixed);
    let tabs = Rect::new(area.x, area.y, area.width, 1.min(area.height));
    let main = Rect::new(area.x, area.y.saturating_add(1), area.width, main_height);

    let panel_visible =
        (show_hive || show_bots) && area.width >= BOTS_MIN_WIDTH && main_height >= 4;
    let (conversation, panel) = if panel_visible {
        let width = bots_width(area.width).min(area.width.saturating_sub(30));
        (
            Rect::new(
                main.x,
                main.y,
                main.width.saturating_sub(width),
                main.height,
            ),
            Some(Rect::new(
                main.x + main.width.saturating_sub(width),
                main.y,
                width,
                main.height,
            )),
        )
    } else {
        (main, None)
    };
    // Split panel vertically between hive and bots.
    let (hive, bots) = if let Some(p) = panel {
        if show_hive && show_bots {
            let hive_height = (p.height / 2).max(3).min(p.height.saturating_sub(3));
            let bots_height = p.height.saturating_sub(hive_height);
            (
                Some(Rect::new(p.x, p.y, p.width, hive_height)),
                Some(Rect::new(p.x, p.y + hive_height, p.width, bots_height)),
            )
        } else if show_hive {
            (Some(p), None)
        } else if show_bots {
            (None, Some(p))
        } else {
            (None, None)
        }
    } else {
        (None, None)
    };

    let mut y = area.y + 1 + main_height;
    let input = Rect::new(area.x, y, area.width, input_height);
    y += input_height;
    let status = Rect::new(area.x, y, area.width, 1);
    y += 1;
    let shortcuts = if shortcuts_height > 0 {
        Some(Rect::new(area.x, y, area.width, 1))
    } else {
        None
    };

    Regions {
        tabs,
        main,
        conversation,
        hive,
        bots,
        input,
        status,
        shortcuts,
        compact,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_wide_terminal_shows_the_bots_panel() {
        let regions = layout(Rect::new(0, 0, 120, 40), true, true, 1);
        assert!(regions.bots.is_some());
        let bots = regions.bots.unwrap();
        assert!(bots.width >= 24 && bots.width <= 40);
        assert_eq!(regions.conversation.width + bots.width, 120);
    }

    #[test]
    fn a_narrow_terminal_collapses_bots_without_losing_the_conversation() {
        let regions = layout(Rect::new(0, 0, 70, 30), true, true, 1);
        assert!(regions.bots.is_none());
        assert_eq!(regions.conversation.width, 70);
    }

    #[test]
    fn a_short_terminal_drops_the_shortcut_bar_but_keeps_the_input() {
        let regions = layout(Rect::new(0, 0, 100, 14), true, true, 1);
        assert!(regions.compact);
        assert!(regions.shortcuts.is_none());
        assert!(regions.input.height >= 3);
        assert!(regions.conversation.height > 0);
    }

    #[test]
    fn the_regions_never_overlap_or_overflow() {
        for width in [40, 70, 88, 120, 200] {
            for height in [10, 14, 24, 50] {
                let area = Rect::new(0, 0, width, height);
                let r = layout(area, true, true, 3);
                assert!(r.tabs.height <= area.height);
                assert!(r.status.y >= r.input.y + r.input.height);
                if let Some(shortcuts) = r.shortcuts {
                    assert!(shortcuts.y + shortcuts.height <= area.height);
                }
            }
        }
    }

    #[test]
    fn input_grows_with_content_up_to_a_cap() {
        let one = layout(Rect::new(0, 0, 120, 40), true, true, 1).input.height;
        let five = layout(Rect::new(0, 0, 120, 40), true, true, 5).input.height;
        let fifty = layout(Rect::new(0, 0, 120, 40), true, true, 50)
            .input
            .height;
        assert_eq!(one, 3);
        assert_eq!(five, 7);
        assert_eq!(fifty, 8);
    }
}
