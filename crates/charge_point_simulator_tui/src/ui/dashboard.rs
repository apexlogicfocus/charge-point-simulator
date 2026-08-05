use ratatui::Frame;
use ratatui::layout::{Constraint, Layout, Rect};
use ratatui::text::{Line, Span};
use ratatui::widgets::{Paragraph, Wrap};

use crate::app::{App, StatusSeverity};
use crate::theme;

/// The named regions of the dashboard screen, computed from the terminal area.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct DashboardLayout {
    pub overview: Rect,
    /// Zero height when the charger has no display - see [`dashboard_layout`]'s `has_display`.
    pub display: Rect,
    pub evse_strip: Rect,
    pub evse_detail: Rect,
    pub log: Rect,
    pub command_bar: Rect,
}

// Each of these panels is a `theme::section` (top-border-only) rather than a four-sided
// `bordered_block`, so it only spends 1 row of its allotted height on chrome instead of 2 -
// hence each being 1 row shorter than it was before this panel/section migration.
const OVERVIEW_HEIGHT: u16 = 2;
const DISPLAY_HEIGHT: u16 = 2;
const EVSE_STRIP_HEIGHT: u16 = 2;
// The 3 rows reclaimed above (1 each from overview/display/evse_strip) are handed entirely to
// the log pane, which was the most starved panel while sitting next to a mostly-empty EVSE
// detail panel: 8 -> 11, taking its visible line count from 6 to 10.
const LOG_HEIGHT: u16 = 11;
const COMMAND_BAR_HEIGHT: u16 = 1;

/// Below this size the dashboard's panels would be squeezed to the point of
/// being unreadable, so the app shows a "resize your terminal" message
/// instead of the normal layout.
pub const MIN_WIDTH: u16 = 60;
pub const MIN_HEIGHT: u16 = 16;

pub fn is_terminal_too_small(area: Rect) -> bool {
    area.width < MIN_WIDTH || area.height < MIN_HEIGHT
}

/// Splits `area` into overview / display / EVSE strip / EVSE detail / log / command bar
/// regions, stacked top to bottom. The display region only reserves space when
/// `has_display` is true (chargers without one skip it entirely, rather than showing an
/// empty panel). The EVSE detail panel takes whatever vertical space is left over, shrinking
/// to nothing rather than panicking when the terminal is too small to fit everything.
pub fn dashboard_layout(area: Rect, has_display: bool) -> DashboardLayout {
    let display_height = if has_display { DISPLAY_HEIGHT } else { 0 };
    let [overview, display, evse_strip, evse_detail, log, command_bar] = Layout::vertical([
        Constraint::Length(OVERVIEW_HEIGHT),
        Constraint::Length(display_height),
        Constraint::Length(EVSE_STRIP_HEIGHT),
        Constraint::Min(0),
        Constraint::Length(LOG_HEIGHT),
        Constraint::Length(COMMAND_BAR_HEIGHT),
    ])
    .areas(area);

    DashboardLayout {
        overview,
        display,
        evse_strip,
        evse_detail,
        log,
        command_bar,
    }
}

pub(super) fn render(frame: &mut Frame, app: &App) {
    let has_display = app
        .charger_state
        .as_ref()
        .is_some_and(|state| state.config.has_display);
    let layout = dashboard_layout(frame.area(), has_display);

    // `focused: false` at every section call site below: panel-level focus (as opposed to the
    // EVSE-strip's own existing focus concept, handled separately) has no representation in
    // `App` yet - that arrives in a later phase, at which point these become real.
    let focused = false;

    if has_display {
        let message = app
            .charger_state
            .as_ref()
            .and_then(|state| state.display_message.as_deref());
        let line = match message {
            Some(message) => Line::styled(message, theme::text()),
            None => Line::styled("(blank)", theme::text_muted()),
        };
        frame.render_widget(Paragraph::new(line).block(theme::section("Display", focused)), layout.display);
    }

    let overview_line = match &app.charger_state {
        Some(state) => Line::from(vec![
            Span::styled(state.config.id.clone(), theme::text()),
            Span::styled("  |  ", theme::text_dim()),
            Span::styled(state.config.ocpp_version.to_string(), theme::text()),
            Span::styled("  |  status: ", theme::text_dim()),
            // Glyph in a fixed two-column field (glyph + one trailing space) so alignment
            // holds whether the terminal renders this Unicode "Ambiguous width" glyph as
            // single- or double-width - see `theme::glyph_field`. Do not drop the space.
            Span::styled(
                theme::glyph_field(theme::connection_glyph(state.connection_status)),
                theme::connection_style(state.connection_status),
            ),
            Span::styled(state.connection_status.to_string(), theme::connection_style(state.connection_status)),
        ]),
        None => Line::styled("no charger selected", theme::text_muted()),
    };
    frame.render_widget(
        Paragraph::new(overview_line).block(theme::section("Overview", focused)),
        layout.overview,
    );

    let evse_strip_line = match &app.charger_state {
        Some(state) if !state.evses.is_empty() => {
            let spans: Vec<Span> = state
                .evses
                .iter()
                .enumerate()
                .flat_map(|(index, evse)| {
                    let label = format!(" EVSE {} ({}) ", evse.id, evse.connectors.len());
                    let style = if index == app.focused_evse { theme::selected() } else { theme::text() };
                    [Span::styled(label, style), Span::raw("  ")]
                })
                .collect();
            Line::from(spans)
        }
        _ => Line::styled("no EVSEs", theme::text_muted()),
    };
    frame.render_widget(
        Paragraph::new(evse_strip_line)
            .block(theme::section("EVSEs", focused))
            .wrap(Wrap { trim: true }),
        layout.evse_strip,
    );

    let evse_detail_title = match &app.charger_state {
        Some(state) if !state.evses.is_empty() => {
            format!("EVSE detail ({}/{})", app.focused_evse + 1, state.evses.len())
        }
        _ => "EVSE detail".to_string(),
    };
    let evse_detail_lines: Vec<Line> = match &app.charger_state {
        Some(state) if !state.evses.is_empty() => {
            let evse = &state.evses[app.focused_evse.min(state.evses.len() - 1)];
            let mut lines = vec![Line::from(vec![
                Span::styled(format!("EVSE {}", evse.id), theme::text()),
                Span::styled("  |  ", theme::text_dim()),
                Span::styled(format!("{:.2}", evse.metrics.power_kw), theme::text()),
                Span::styled(" kW  ", theme::text_dim()),
                Span::styled(format!("{:.1}", evse.metrics.current_a), theme::text()),
                Span::styled(" A  ", theme::text_dim()),
                Span::styled(format!("{:.3}", evse.metrics.energy_kwh), theme::text()),
                Span::styled(" kWh", theme::text_dim()),
            ])];
            lines.extend(evse.connectors.iter().map(|connector| {
                let vehicle = connector
                    .vehicle
                    .as_ref()
                    .map(|vehicle| format!(" - {}", vehicle.id))
                    .unwrap_or_default();
                Line::from(vec![
                    // Same fixed two-column glyph field as the overview line above.
                    Span::styled(
                        theme::glyph_field(theme::connector_glyph(connector.status)),
                        theme::connector_style(connector.status),
                    ),
                    Span::styled(format!("connector {}: ", connector.id), theme::text_dim()),
                    Span::styled(connector.status.to_string(), theme::connector_style(connector.status)),
                    Span::styled(vehicle, theme::text()),
                ])
            }));
            lines
        }
        Some(_) => vec![Line::styled("no EVSEs configured", theme::text_muted())],
        None => vec![Line::styled("no charger selected", theme::text_muted())],
    };
    frame.render_widget(
        Paragraph::new(evse_detail_lines).block(theme::section(evse_detail_title, focused)),
        layout.evse_detail,
    );

    let log_title = if app.logs.is_paused() {
        "Logs (scrolled up, paused)"
    } else {
        "Logs"
    };
    // A `section` only spends 1 row on chrome (the top rule), not 2, so all but 1 row of
    // `layout.log`'s height is available for content.
    let log_height = layout.log.height.saturating_sub(1) as usize;
    let log_lines: Vec<Line> = app
        .logs
        .visible_lines(log_height)
        .into_iter()
        .map(|line| Line::styled(line, theme::text()))
        .collect();
    frame.render_widget(
        Paragraph::new(log_lines).block(theme::section(log_title, focused)),
        layout.log,
    );

    let command_bar_line = match &app.status_message {
        Some((severity, message)) => {
            let style = match severity {
                StatusSeverity::Ok => theme::ok(),
                StatusSeverity::Error => theme::error(),
            };
            Line::styled(message.clone(), style)
        }
        None => Line::styled(
            "q: quit  Esc: back  ←/→/Tab: focus EVSE  PgUp/PgDn: scroll logs  c: command  ?: help",
            theme::text_dim(),
        ),
    };
    frame.render_widget(Paragraph::new(command_bar_line), layout.command_bar);
}

#[cfg(test)]
mod tests {
    use super::*;

    fn area(width: u16, height: u16) -> Rect {
        Rect::new(0, 0, width, height)
    }

    #[test]
    fn stacks_regions_top_to_bottom_in_order() {
        let layout = dashboard_layout(area(80, 40), true);

        assert_eq!(layout.overview.y, 0);
        assert_eq!(layout.display.y, layout.overview.bottom());
        assert_eq!(layout.evse_strip.y, layout.display.bottom());
        assert_eq!(layout.evse_detail.y, layout.evse_strip.bottom());
        assert_eq!(layout.log.y, layout.evse_detail.bottom());
        assert_eq!(layout.command_bar.y, layout.log.bottom());
        assert_eq!(layout.command_bar.bottom(), 40);
    }

    #[test]
    fn fixed_regions_use_their_configured_height_when_space_allows() {
        let layout = dashboard_layout(area(80, 40), true);

        assert_eq!(layout.overview.height, OVERVIEW_HEIGHT);
        assert_eq!(layout.display.height, DISPLAY_HEIGHT);
        assert_eq!(layout.evse_strip.height, EVSE_STRIP_HEIGHT);
        assert_eq!(layout.log.height, LOG_HEIGHT);
        assert_eq!(layout.command_bar.height, COMMAND_BAR_HEIGHT);
    }

    #[test]
    fn the_display_region_takes_no_space_when_the_charger_has_no_display() {
        let layout = dashboard_layout(area(80, 40), false);

        assert_eq!(layout.display.height, 0);
        assert_eq!(layout.evse_strip.y, layout.overview.bottom());
    }

    #[test]
    fn evse_detail_absorbs_the_remaining_space() {
        let layout = dashboard_layout(area(80, 40), true);
        let fixed_height =
            OVERVIEW_HEIGHT + DISPLAY_HEIGHT + EVSE_STRIP_HEIGHT + LOG_HEIGHT + COMMAND_BAR_HEIGHT;

        assert_eq!(layout.evse_detail.height, 40 - fixed_height);
    }

    #[test]
    fn regions_span_the_full_width() {
        let layout = dashboard_layout(area(80, 40), true);

        for region in [
            layout.overview,
            layout.display,
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
        let layout = dashboard_layout(area(80, 2), true);

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
