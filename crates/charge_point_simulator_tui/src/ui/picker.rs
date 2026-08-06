use ratatui::Frame;
use ratatui::layout::{Constraint, Layout, Rect};
use ratatui::style::{Modifier, Style};
use ratatui::text::Line;
use ratatui::widgets::{Cell, Paragraph, Row, Table, TableState};
use tui_big_text::{BigText, PixelSize};

use crate::app::App;
use crate::theme::{self, BRAND_TEAL};
use charge_point_simulator_core::charger::{ChargerEntry, ChargerSource};

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

/// A charger row's "source" column: `"built-in"` for a preset, or the bare YAML file name for
/// one discovered on disk - so a user with several similarly-named configured chargers can
/// tell them apart without leaving the picker.
fn source_label(source: &ChargerSource) -> String {
    match source {
        ChargerSource::BuiltIn => "built-in".to_string(),
        ChargerSource::Configured { file_name } => file_name.clone(),
    }
}

/// The last-used CSMS endpoint for `entry`, from [`App::connection_store`] - a stand-in for "if
/// I press Enter, what will this actually connect to." Chargers never connected before (or
/// whose OCPP version never reaches the connection setup screen) show a muted placeholder
/// instead of an empty cell.
fn last_endpoint_label(app: &App, entry: &ChargerEntry) -> String {
    app.connection_store
        .get(&entry.config.id)
        .map(|profile| profile.csms_url.clone())
        .filter(|url| !url.trim().is_empty())
        .unwrap_or_else(|| "—".to_string())
}

/// Truncates `text` to at most `max_width` character cells, appending `…` when something was
/// actually cut - so a column too narrow for a long charger id, YAML file name, or CSMS URL
/// says so, instead of `ratatui::widgets::Table` silently clipping it mid-word on its own.
fn truncate_with_ellipsis(text: &str, max_width: u16) -> String {
    let max_width = max_width as usize;
    if text.chars().count() <= max_width {
        return text.to_string();
    }
    if max_width == 0 {
        return String::new();
    }
    let mut truncated: String = text.chars().take(max_width - 1).collect();
    truncated.push('…');
    truncated
}

/// The actual rendered width of each column in `widths` at `area_width`, replicating exactly
/// what `Table::get_column_widths` computes internally, so [`truncate_with_ellipsis`] cuts cell
/// text at the width it will really occupy rather than an estimate that could drift from it.
fn column_widths(
    area_width: u16,
    widths: &[Constraint],
    selection_width: u16,
    spacing: u16,
) -> Vec<u16> {
    let [_selection_area, columns_area] =
        Layout::horizontal([Constraint::Length(selection_width), Constraint::Fill(0)])
            .areas(Rect::new(0, 0, area_width, 1));
    Layout::horizontal(widths)
        .spacing(spacing)
        .split(columns_area)
        .iter()
        .map(|rect| rect.width)
        .collect()
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

    let block = theme::section("Select a charger", focused);

    if chargers.is_empty() {
        frame.render_widget(
            Paragraph::new(Line::styled("no chargers match", theme::text_muted())).block(block),
            list_area,
        );
        return;
    }

    const COLUMN_SPACING: u16 = 2;
    const HIGHLIGHT_SYMBOL: &str = "> ";

    // Weighted 2:2:3 rather than even, so the two columns most likely to hold something long
    // (a YAML file name, a full CSMS URL) get more of the shrink-to-fit room than the charger
    // id typically needs.
    let widths = [
        Constraint::Fill(2),
        Constraint::Length(10),
        Constraint::Length(8),
        Constraint::Fill(2),
        Constraint::Fill(3),
    ];

    // The block only borders the top (see `theme::section`), so it doesn't narrow `list_area` -
    // computing against `list_area.width` directly matches what the `Table` below will actually
    // render at.
    let col_widths = column_widths(
        list_area.width,
        &widths,
        HIGHLIGHT_SYMBOL.chars().count() as u16,
        COLUMN_SPACING,
    );

    let header = Row::new(
        ["Charger", "OCPP", "EVSEs", "Source", "Last endpoint"]
            .map(|title| Cell::from(Line::styled(title, theme::text_dim()))),
    );

    let rows = chargers.iter().map(|entry| {
        let evse_count = entry.config.evses.len();
        let cells = [
            entry.config.id.clone(),
            entry.config.ocpp_version.to_string(),
            format!(
                "{evse_count} EVSE{}",
                if evse_count == 1 { "" } else { "s" }
            ),
            source_label(&entry.source),
            last_endpoint_label(app, entry),
        ];
        Row::new(
            cells
                .into_iter()
                .zip(col_widths.iter())
                .map(|(text, &width)| {
                    Cell::from(Line::styled(
                        truncate_with_ellipsis(&text, width),
                        theme::text(),
                    ))
                }),
        )
    });

    let table = Table::new(rows, widths)
        .header(header)
        .column_spacing(COLUMN_SPACING)
        .block(block)
        .row_highlight_style(theme::selected())
        .highlight_symbol(HIGHLIGHT_SYMBOL);

    let mut state = TableState::default().with_selected(Some(app.selected_charger));

    frame.render_stateful_widget(table, list_area, &mut state);
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
