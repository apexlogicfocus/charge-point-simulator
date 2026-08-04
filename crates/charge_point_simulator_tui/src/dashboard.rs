use ratatui::layout::{Constraint, Layout, Rect};

/// The named regions of the dashboard screen, computed from the terminal area.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct DashboardLayout {
    pub overview: Rect,
    pub evse_strip: Rect,
    pub evse_detail: Rect,
    pub log: Rect,
    pub command_bar: Rect,
}

const OVERVIEW_HEIGHT: u16 = 3;
const EVSE_STRIP_HEIGHT: u16 = 3;
const LOG_HEIGHT: u16 = 8;
const COMMAND_BAR_HEIGHT: u16 = 1;

/// Below this size the dashboard's panels would be squeezed to the point of
/// being unreadable, so the app shows a "resize your terminal" message
/// instead of the normal layout.
pub const MIN_WIDTH: u16 = 60;
pub const MIN_HEIGHT: u16 = 16;

pub fn is_terminal_too_small(area: Rect) -> bool {
    area.width < MIN_WIDTH || area.height < MIN_HEIGHT
}

/// Splits `area` into overview / EVSE strip / EVSE detail / log / command bar
/// regions, stacked top to bottom. The EVSE detail panel takes whatever
/// vertical space is left over, shrinking to nothing rather than panicking
/// when the terminal is too small to fit everything.
pub fn dashboard_layout(area: Rect) -> DashboardLayout {
    let [overview, evse_strip, evse_detail, log, command_bar] = Layout::vertical([
        Constraint::Length(OVERVIEW_HEIGHT),
        Constraint::Length(EVSE_STRIP_HEIGHT),
        Constraint::Min(0),
        Constraint::Length(LOG_HEIGHT),
        Constraint::Length(COMMAND_BAR_HEIGHT),
    ])
    .areas(area);

    DashboardLayout {
        overview,
        evse_strip,
        evse_detail,
        log,
        command_bar,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn area(width: u16, height: u16) -> Rect {
        Rect::new(0, 0, width, height)
    }

    #[test]
    fn stacks_regions_top_to_bottom_in_order() {
        let layout = dashboard_layout(area(80, 40));

        assert_eq!(layout.overview.y, 0);
        assert_eq!(layout.evse_strip.y, layout.overview.bottom());
        assert_eq!(layout.evse_detail.y, layout.evse_strip.bottom());
        assert_eq!(layout.log.y, layout.evse_detail.bottom());
        assert_eq!(layout.command_bar.y, layout.log.bottom());
        assert_eq!(layout.command_bar.bottom(), 40);
    }

    #[test]
    fn fixed_regions_use_their_configured_height_when_space_allows() {
        let layout = dashboard_layout(area(80, 40));

        assert_eq!(layout.overview.height, OVERVIEW_HEIGHT);
        assert_eq!(layout.evse_strip.height, EVSE_STRIP_HEIGHT);
        assert_eq!(layout.log.height, LOG_HEIGHT);
        assert_eq!(layout.command_bar.height, COMMAND_BAR_HEIGHT);
    }

    #[test]
    fn evse_detail_absorbs_the_remaining_space() {
        let layout = dashboard_layout(area(80, 40));
        let fixed_height = OVERVIEW_HEIGHT + EVSE_STRIP_HEIGHT + LOG_HEIGHT + COMMAND_BAR_HEIGHT;

        assert_eq!(layout.evse_detail.height, 40 - fixed_height);
    }

    #[test]
    fn regions_span_the_full_width() {
        let layout = dashboard_layout(area(80, 40));

        for region in [
            layout.overview,
            layout.evse_strip,
            layout.evse_detail,
            layout.log,
            layout.command_bar,
        ] {
            assert_eq!(region.width, 80);
        }
    }

    #[test]
    fn shrinks_gracefully_when_the_terminal_is_too_small() {
        let layout = dashboard_layout(area(80, 2));

        assert_eq!(layout.evse_detail.height, 0);
        // Layout still fits within the given area instead of panicking or overflowing.
        assert!(layout.command_bar.bottom() <= 2);
    }

    #[test]
    fn a_comfortably_sized_terminal_is_not_too_small() {
        assert!(!is_terminal_too_small(area(80, 30)));
    }

    #[test]
    fn a_terminal_narrower_than_the_minimum_is_too_small() {
        assert!(is_terminal_too_small(area(MIN_WIDTH - 1, 30)));
    }

    #[test]
    fn a_terminal_shorter_than_the_minimum_is_too_small() {
        assert!(is_terminal_too_small(area(80, MIN_HEIGHT - 1)));
    }

    #[test]
    fn the_minimum_size_itself_is_not_too_small() {
        assert!(!is_terminal_too_small(area(MIN_WIDTH, MIN_HEIGHT)));
    }
}
