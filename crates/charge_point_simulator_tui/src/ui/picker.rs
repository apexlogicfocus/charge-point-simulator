use ratatui::Frame;
use ratatui::layout::{Constraint, Layout, Rect};
use ratatui::style::{Modifier, Style};
use ratatui::text::Line;
use ratatui::widgets::{List, ListItem, ListState, Paragraph};
use tui_big_text::{BigText, PixelSize};

use crate::app::App;
use crate::theme::{self, BRAND_TEAL};

/// Rows occupied by the stacked "CHARGE" / "POINT" / "SIMULATOR" banner at
/// `PixelSize::Quadrant` (4 terminal rows per glyph line).
const BANNER_HEIGHT: u16 = 3 * 4;

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

pub(super) fn render(frame: &mut Frame, app: &App) {
    let layout = picker_layout(frame.area());
    render_banner(frame, layout.banner);
    render_charger_list(frame, app, layout.list);
}

fn render_banner(frame: &mut Frame, area: Rect) {
    let [_, banner, _] = Layout::vertical([
        Constraint::Fill(1),
        Constraint::Length(BANNER_HEIGHT),
        Constraint::Fill(1),
    ])
    .areas(area);

    let big_text = BigText::builder()
        .pixel_size(PixelSize::Quadrant)
        .style(Style::new().fg(BRAND_TEAL).add_modifier(Modifier::BOLD))
        .centered()
        .lines(vec!["CHARGE".into(), "POINT".into(), "SIMULATOR".into()])
        .build();

    frame.render_widget(big_text, banner);
}

fn render_charger_list(frame: &mut Frame, app: &App, area: Rect) {
    let chargers = app.filtered_chargers();
    // 2, not 3: the filter box is now a `theme::section` (1 row of chrome, the top rule) with
    // 1 content row, not a 4-sided `bordered_block` (2 rows of chrome). The freed row goes to
    // the list below via `Fill(1)`.
    let [filter_area, list_area] =
        Layout::vertical([Constraint::Length(2), Constraint::Fill(1)]).areas(area);

    // `focused: false`: the picker has no panel-focus concept yet (see the same note in
    // `dashboard.rs`) - the filter box is always the de facto input target on this screen.
    let focused = false;

    frame.render_widget(
        Paragraph::new(Line::styled(app.picker_filter.value(), theme::text()))
            .block(theme::section("Filter", focused)),
        filter_area,
    );
    frame.set_cursor_position((
        filter_area.x + app.picker_filter.cursor() as u16,
        filter_area.y + 1,
    ));

    let items: Vec<ListItem> = if chargers.is_empty() {
        vec![ListItem::new(Line::styled(
            "no chargers match",
            theme::text_muted(),
        ))]
    } else {
        chargers
            .iter()
            .map(|entry| {
                let evse_count = entry.config.evses.len();
                ListItem::new(Line::styled(
                    format!(
                        "{}  [{}]  {} EVSE{}",
                        entry.config.id,
                        entry.config.ocpp_version,
                        evse_count,
                        if evse_count == 1 { "" } else { "s" }
                    ),
                    theme::text(),
                ))
            })
            .collect()
    };

    let list = List::new(items)
        .block(theme::section("Select a charger", focused))
        .highlight_style(theme::selected())
        .highlight_symbol("> ");

    let mut state = ListState::default();
    if !chargers.is_empty() {
        state.select(Some(app.selected_charger));
    }

    frame.render_stateful_widget(list, list_area, &mut state);
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
