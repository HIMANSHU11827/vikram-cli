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
    /// One line of feedback directly above the input. `None` when nothing is happening.
    pub notice: Option<Rect>,
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

pub fn layout(
    area: Rect,
    show_hive: bool,
    show_bots: bool,
    input_lines: usize,
    show_notice: bool,
) -> Regions {
    let compact = area.height < BOTS_COLLAPSE_HEIGHT;
    let input_height = ((input_lines.max(1) as u16) + 2).clamp(3, 8);
    let shortcuts_height: u16 = if compact { 0 } else { 1 };
    let fixed = 1 + input_height + 1 + shortcuts_height;

    // The notice line is owed whenever the app has something to say, but it must never cost the
    // conversation its last rows: on a short terminal the message is what gets dropped.
    let show_notice = show_notice && area.height.saturating_sub(fixed + 1) >= 3;
    let fixed = fixed + u16::from(show_notice);

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
    let notice = if show_notice {
        let rect = Rect::new(area.x, y, area.width, 1);
        y += 1;
        Some(rect)
    } else {
        None
    };
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
        notice,
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
        let regions = layout(Rect::new(0, 0, 120, 40), true, true, 1, false);
        assert!(regions.bots.is_some());
        let bots = regions.bots.unwrap();
        assert!(bots.width >= 24 && bots.width <= 40);
        assert_eq!(regions.conversation.width + bots.width, 120);
    }

    #[test]
    fn a_narrow_terminal_collapses_bots_without_losing_the_conversation() {
        let regions = layout(Rect::new(0, 0, 70, 30), true, true, 1, false);
        assert!(regions.bots.is_none());
        assert_eq!(regions.conversation.width, 70);
    }

    #[test]
    fn a_short_terminal_drops_the_shortcut_bar_but_keeps_the_input() {
        let regions = layout(Rect::new(0, 0, 100, 14), true, true, 1, false);
        assert!(regions.compact);
        assert!(regions.shortcuts.is_none());
        assert!(regions.input.height >= 3);
        assert!(regions.conversation.height > 0);
    }

    #[test]
    fn the_notice_line_sits_directly_above_the_input() {
        let regions = layout(Rect::new(0, 0, 120, 40), true, true, 1, true);
        let notice = regions.notice.expect("a notice was asked for");
        assert_eq!(notice.height, 1);
        assert_eq!(notice.y + 1, regions.input.y);
        assert_eq!(notice.width, 120);
        assert_eq!(
            regions.main.height,
            layout(Rect::new(0, 0, 120, 40), true, true, 1, false)
                .main
                .height
                - 1
        );
    }

    #[test]
    fn a_short_terminal_refuses_to_spend_a_row_on_the_notice() {
        let regions = layout(Rect::new(0, 0, 100, 8), true, true, 1, true);
        assert!(regions.notice.is_none());
        assert!(regions.conversation.height >= 3);
    }

    #[test]
    fn the_regions_never_overlap_or_overflow() {
        for width in [40, 70, 88, 120, 200] {
            for height in [10, 14, 24, 50] {
                for show_notice in [false, true] {
                    let area = Rect::new(0, 0, width, height);
                    let r = layout(area, true, true, 3, show_notice);
                    assert!(r.tabs.height <= area.height);
                    assert!(r.status.y >= r.input.y + r.input.height);
                    if let Some(notice) = r.notice {
                        assert!(notice.y + notice.height <= r.input.y);
                    }
                    if let Some(shortcuts) = r.shortcuts {
                        assert!(shortcuts.y + shortcuts.height <= area.height);
                    }
                }
            }
        }
    }

    #[test]
    fn input_grows_with_content_up_to_a_cap() {
        let one = layout(Rect::new(0, 0, 120, 40), true, true, 1, false)
            .input
            .height;
        let five = layout(Rect::new(0, 0, 120, 40), true, true, 5, false)
            .input
            .height;
        let fifty = layout(Rect::new(0, 0, 120, 40), true, true, 50, false)
            .input
            .height;
        assert_eq!(one, 3);
        assert_eq!(five, 7);
        assert_eq!(fifty, 8);
    }
}
