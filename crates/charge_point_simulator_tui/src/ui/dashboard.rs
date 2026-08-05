use std::time::Duration;

use ratatui::Frame;
use ratatui::layout::{Constraint, Layout, Rect};
use ratatui::style::Style;
use ratatui::text::{Line, Span};
use ratatui::widgets::{Paragraph, Wrap};

use crate::app::{App, StatusSeverity};
use crate::theme;
use charge_point_simulator_core::charger::{ChargerState, ConnectionStatus, SimulationMode};

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
//
// `overview` itself is the exception: it's a `theme::header` (bottom-border-only), not a
// `section` - its rule sits below the header line rather than carrying a title above it (see
// `theme::header`'s doc comment). It still only spends 1 of its 2 rows on chrome, for the same
// reason as everything else here.
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

/// Animation frames for the "connecting..." indicator shown in the header while a CSMS
/// connection attempt is pending (see `App::connect_result_receiver`). Plain ASCII rather than
/// one of `theme`'s status glyphs on purpose: this is decorative motion, not a state that needs
/// to stay legible without color/animation on a monochrome or piped terminal - the adjacent
/// "connecting..." text already carries that meaning by itself.
const SPINNER_FRAMES: [&str; 4] = ["|", "/", "-", "\\"];

/// How long each spinner frame is shown, in milliseconds.
const SPINNER_FRAME_MS: u128 = 250;

/// Picks the spinner frame for `uptime` - the charger's *simulated* elapsed time, deliberately
/// never `Instant::now()`. Uptime only advances via `ChargerState::tick`, which snapshot tests
/// drive with fixed, hand-chosen durations, so a golden built from a given uptime always
/// reproduces the same frame. Wall-clock time would make the same golden flaky depending on how
/// fast the test happened to run.
fn connecting_spinner_frame(uptime: Duration) -> &'static str {
    let index = (uptime.as_millis() / SPINNER_FRAME_MS) as usize % SPINNER_FRAMES.len();
    SPINNER_FRAMES[index]
}

/// Compact uptime formatting for the header: `"0s"` while brand new, `"42s"` under a minute,
/// `"4m 12s"` under an hour, `"1h 04m"` from there on. Each tier only carries the units a user
/// actually needs at that scale - nobody needs "0h" prefixed on "4m 12s", or seconds once a
/// session has run for hours.
fn format_uptime(uptime: Duration) -> String {
    let total_secs = uptime.as_secs();
    let hours = total_secs / 3600;
    let minutes = (total_secs % 3600) / 60;
    let seconds = total_secs % 60;
    if hours > 0 {
        format!("{hours}h {minutes:02}m")
    } else if minutes > 0 {
        format!("{minutes}m {seconds:02}s")
    } else {
        format!("{seconds}s")
    }
}

/// What the header's mode segment reads: a plain label for a local simulation, or the CSMS URL
/// itself when actually driving one. This is the single most important thing the header adds
/// over the old "Overview" panel - previously nothing on the dashboard told a user whether
/// their commands were reaching a real CSMS or just mutating local state.
fn mode_text(state: &ChargerState) -> String {
    match &state.mode {
        SimulationMode::Local => "local simulation".to_string(),
        SimulationMode::LiveCsms { url } => url.clone(),
    }
}

/// Builds the header line's content as `(text, style)` segments, laid out left to right and
/// trimmed to fit `width` columns.
///
/// Priority for what gets dropped first when space is tight, lowest first:
/// 1. **uptime** - a nicety, the first thing dropped.
/// 2. **mode** - whether commands actually reach a CSMS, and which one; the single most
///    important addition this header makes, so it's kept as long as there's any room for it.
/// 3. **status glyph/label, charger id, and OCPP version** - the floor, never dropped. Without
///    these there's no way to tell which charger is even on screen or whether it's healthy.
fn header_segments(state: &ChargerState, connecting: bool, width: usize) -> Vec<(String, Style)> {
    // While a connection attempt is pending, an animated spinner (driven by simulated
    // `uptime`, never wall-clock time - see `connecting_spinner_frame`) replaces the normal
    // status glyph: `connection_status` itself is still `Booting` at this point (the OCPP
    // bridge hasn't reported anything yet), so without this the header would sit static and
    // give no feedback that anything is happening.
    let status: Vec<(String, Style)> = if connecting {
        let connecting_style = theme::connection_style(ConnectionStatus::Booting);
        vec![
            (theme::glyph_field(connecting_spinner_frame(state.uptime)), connecting_style),
            ("connecting...".to_string(), connecting_style),
        ]
    } else {
        vec![
            (
                theme::glyph_field(theme::connection_glyph(state.connection_status)),
                theme::connection_style(state.connection_status),
            ),
            (state.connection_status.to_string(), theme::connection_style(state.connection_status)),
        ]
    };

    let mut base = status;
    base.push(("  |  ".to_string(), theme::text_dim()));
    base.push((state.config.id.clone(), theme::text()));
    base.push(("  |  ".to_string(), theme::text_dim()));
    base.push((state.config.ocpp_version.to_string(), theme::text_dim()));

    let mode_segment = vec![("  |  ".to_string(), theme::text_dim()), (mode_text(state), theme::text())];

    let uptime_segment = vec![
        ("  |  up ".to_string(), theme::text_dim()),
        (format_uptime(state.uptime), theme::text_dim()),
    ];

    // Widths are counted in `char`s, not bytes, matching `theme::glyph_field`'s convention -
    // every segment used here is either fixed-width ASCII or a `glyph_field` whose width is
    // already normalized to exactly two columns, so `chars().count()` and on-screen column
    // count agree.
    let width_of = |segments: &[(String, Style)]| -> usize {
        segments.iter().map(|(text, _)| text.chars().count()).sum()
    };

    let with_mode: Vec<(String, Style)> = base.iter().cloned().chain(mode_segment).collect();
    let with_mode_and_uptime: Vec<(String, Style)> = with_mode.iter().cloned().chain(uptime_segment).collect();

    if width_of(&with_mode_and_uptime) <= width {
        with_mode_and_uptime
    } else if width_of(&with_mode) <= width {
        with_mode
    } else {
        base
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

    let header_line = match &app.charger_state {
        Some(state) => {
            let connecting = app.connect_result_receiver.is_some();
            let segments = header_segments(state, connecting, layout.overview.width as usize);
            Line::from(
                segments
                    .into_iter()
                    .map(|(text, style)| Span::styled(text, style))
                    .collect::<Vec<_>>(),
            )
        }
        None => Line::styled("no charger selected", theme::text_muted()),
    };
    frame.render_widget(Paragraph::new(header_line).block(theme::header(focused)), layout.overview);

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

    // --- format_uptime ---------------------------------------------------------------

    #[test]
    fn format_uptime_at_zero_reads_as_zero_seconds() {
        assert_eq!(format_uptime(Duration::ZERO), "0s");
    }

    #[test]
    fn format_uptime_under_a_minute_shows_seconds_only() {
        assert_eq!(format_uptime(Duration::from_secs(42)), "42s");
    }

    #[test]
    fn format_uptime_under_an_hour_shows_minutes_and_seconds() {
        assert_eq!(format_uptime(Duration::from_secs(4 * 60 + 12)), "4m 12s");
    }

    #[test]
    fn format_uptime_an_hour_or_more_shows_hours_and_minutes() {
        assert_eq!(format_uptime(Duration::from_secs(3600 + 4 * 60)), "1h 04m");
    }

    // --- connecting_spinner_frame ------------------------------------------------------

    #[test]
    fn connecting_spinner_frame_cycles_through_frames_as_simulated_uptime_advances() {
        assert_eq!(connecting_spinner_frame(Duration::ZERO), "|");
        assert_eq!(connecting_spinner_frame(Duration::from_millis(250)), "/");
        assert_eq!(connecting_spinner_frame(Duration::from_millis(500)), "-");
        assert_eq!(connecting_spinner_frame(Duration::from_millis(750)), "\\");
    }

    #[test]
    fn connecting_spinner_frame_wraps_around_after_the_last_frame() {
        assert_eq!(connecting_spinner_frame(Duration::from_millis(1000)), "|");
    }

    // --- header_segments ---------------------------------------------------------------

    use charge_point_simulator_core::charger::{ChargerConfig, OcppVersion};

    fn charger_state_for_header(id: &str) -> ChargerState {
        ChargerState::from_config(ChargerConfig {
            id: id.to_string(),
            ocpp_version: OcppVersion::V16J,
            evses: vec![],
            has_display: false,
        })
    }

    fn segments_text(segments: &[(String, Style)]) -> String {
        segments.iter().map(|(text, _)| text.as_str()).collect()
    }

    #[test]
    fn header_segments_show_local_simulation_for_a_local_charger() {
        let state = charger_state_for_header("CP-CHARGE");
        let text = segments_text(&header_segments(&state, false, 120));

        assert!(text.contains("CP-CHARGE"));
        assert!(text.contains("local simulation"));
    }

    #[test]
    fn header_segments_show_the_csms_url_for_a_live_csms_charger() {
        let mut state = charger_state_for_header("CP-CHARGE");
        state.mode = SimulationMode::LiveCsms { url: "wss://csms.example.com".to_string() };
        let text = segments_text(&header_segments(&state, false, 120));

        assert!(text.contains("wss://csms.example.com"));
        assert!(!text.contains("local simulation"));
    }

    #[test]
    fn header_segments_show_a_spinner_and_connecting_while_a_connect_attempt_is_pending() {
        let state = charger_state_for_header("CP-CHARGE");
        let text = segments_text(&header_segments(&state, true, 120));

        assert!(text.contains("connecting..."));
        // The normal connection-status label (the fresh charger is `Booting`) is replaced,
        // not merely joined by, the spinner - there's only one status slot in the header.
        assert!(!text.contains("booting"));
    }

    #[test]
    fn header_segments_drop_uptime_before_mode_and_mode_before_the_floor_as_width_shrinks() {
        // Priority (see `header_segments`'s doc comment): uptime drops first, then mode; the
        // status/id/version floor is never dropped. Derive the exact boundary widths from the
        // same building blocks `header_segments` itself uses, rather than hand-counting
        // characters, so this test doesn't silently drift from the real format strings.
        let mut state = charger_state_for_header("CP-CHARGE");
        state.uptime = Duration::from_secs(4 * 60 + 12);

        // A width of 0 always returns just the never-dropped floor.
        let base = header_segments(&state, false, 0);
        let base_width: usize = base.iter().map(|(text, _)| text.chars().count()).sum();

        let mode_only_width = base_width + "  |  ".chars().count() + mode_text(&state).chars().count();
        let with_uptime_width =
            mode_only_width + "  |  up ".chars().count() + format_uptime(state.uptime).chars().count();

        let everything = segments_text(&header_segments(&state, false, with_uptime_width));
        assert!(everything.contains("local simulation"));
        assert!(everything.contains("up 4m 12s"));

        let mode_only = segments_text(&header_segments(&state, false, with_uptime_width - 1));
        assert!(mode_only.contains("local simulation"), "mode should still fit: {mode_only:?}");
        assert!(!mode_only.contains("up 4m 12s"), "uptime should have been dropped: {mode_only:?}");

        let floor_only = segments_text(&header_segments(&state, false, mode_only_width - 1));
        assert!(!floor_only.contains("local simulation"), "mode should have been dropped: {floor_only:?}");
        assert!(floor_only.contains("CP-CHARGE"), "the id/status floor must never be dropped: {floor_only:?}");
    }
}
