use ratatui::layout::{Constraint, Layout, Rect};

/// The named regions of the charger picker screen: a banner on the left, the
/// selectable charger list on the right.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct PickerLayout {
    pub banner: Rect,
    pub list: Rect,
}

/// Wide enough to fit "SIMULATOR" (the widest stacked banner word) rendered
/// with `PixelSize::Quadrant` (4 terminal columns per glyph).
const BANNER_WIDTH: u16 = 40;

/// Splits `area` into a fixed-width banner on the left and the charger list
/// filling the rest. On a narrow terminal the list shrinks toward nothing
/// rather than panicking.
pub fn picker_layout(area: Rect) -> PickerLayout {
    let [banner, list] =
        Layout::horizontal([Constraint::Length(BANNER_WIDTH), Constraint::Min(0)]).areas(area);

    PickerLayout { banner, list }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn area(width: u16, height: u16) -> Rect {
        Rect::new(0, 0, width, height)
    }

    #[test]
    fn banner_sits_left_of_the_list() {
        let layout = picker_layout(area(100, 30));

        assert_eq!(layout.banner.x, 0);
        assert_eq!(layout.list.x, layout.banner.right());
    }

    #[test]
    fn banner_uses_its_fixed_width_when_space_allows() {
        let layout = picker_layout(area(100, 30));
        assert_eq!(layout.banner.width, BANNER_WIDTH);
    }

    #[test]
    fn list_takes_the_remaining_width() {
        let layout = picker_layout(area(100, 30));
        assert_eq!(layout.list.width, 100 - BANNER_WIDTH);
    }

    #[test]
    fn both_regions_span_the_full_height() {
        let layout = picker_layout(area(100, 30));
        assert_eq!(layout.banner.height, 30);
        assert_eq!(layout.list.height, 30);
    }

    #[test]
    fn list_shrinks_rather_than_panicking_on_a_narrow_terminal() {
        let layout = picker_layout(area(20, 30));
        assert_eq!(layout.list.width, 0);
        assert!(layout.banner.width <= 20);
    }
}
