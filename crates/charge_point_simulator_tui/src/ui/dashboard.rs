use std::time::Duration;

use ratatui::Frame;
use ratatui::layout::{Constraint, Layout, Rect};
use ratatui::style::Style;
use ratatui::text::{Line, Span};
use ratatui::widgets::{Paragraph, Scrollbar, ScrollbarOrientation, ScrollbarState};

use super::view::DashboardView;
use crate::app::{FocusedConnector, StatusSeverity};
use crate::logs::{Direction, LogLevel};
use crate::theme;
use charge_point_simulator_core::charger::{
    ChargerState, ConnectionStatus, ConnectorState, ConnectorStatus, EvseState, SimulationMode,
};

/// The named top-level regions of the dashboard screen, computed from the terminal area: a
/// header line, a body (the EVSE/connector tree, and a detail sidebar when there's room for
/// one - see [`body_layout`]), the log pane, and the command bar.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct DashboardLayout {
    pub header: Rect,
    pub body: Rect,
    pub log: Rect,
    pub command_bar: Rect,
}

/// The body's own sub-regions, split out from [`DashboardLayout::body`] by [`body_layout`]:
/// the (optional) display-message strip, the EVSE/connector tree, and the detail sidebar -
/// `None` when the terminal is too narrow for one (see [`SIDEBAR_MIN_BODY_WIDTH`]), in which
/// case the focused connector's detail is rendered inline in the tree instead (see
/// [`render`]/[`tree_lines`]'s `inline_detail` parameter).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct BodyLayout {
    /// Zero height when the charger has no display - see [`body_layout`]'s `has_display`.
    pub display: Rect,
    pub tree: Rect,
    pub sidebar: Option<Rect>,
}

// Each of these panels is a `theme::section` (top-border-only) rather than a four-sided
// `bordered_block`, so it only spends 1 row of its allotted height on chrome instead of 2.
//
// `header` itself is the exception: it's a `theme::header` (bottom-border-only), not a
// `section` - its rule sits below the header line rather than carrying a title above it (see
// `theme::header`'s doc comment).
const HEADER_HEIGHT: u16 = 2;
const DISPLAY_HEIGHT: u16 = 2;
const LOG_HEIGHT: u16 = 11;
const COMMAND_BAR_HEIGHT: u16 = 1;

/// The detail sidebar's fixed width when there's room for one.
const SIDEBAR_WIDTH: u16 = 32;

/// Below this body width, the sidebar collapses and the focused connector's detail is shown
/// inline beneath its row in the tree instead - see [`body_layout`]. Chosen so the sidebar
/// only ever appears where it can sit comfortably next to a tree that still has room to
/// breathe (the 80-column golden below this threshold relies on the inline path instead).
const SIDEBAR_MIN_BODY_WIDTH: u16 = 100;

/// Below this size the dashboard's panels would be squeezed to the point of
/// being unreadable, so the app shows a "resize your terminal" message
/// instead of the normal layout.
pub const MIN_WIDTH: u16 = 60;
pub const MIN_HEIGHT: u16 = 16;

pub fn is_terminal_too_small(area: Rect) -> bool {
    area.width < MIN_WIDTH || area.height < MIN_HEIGHT
}

/// Splits `area` into header / body / log / command bar regions, stacked top to bottom. The
/// body takes whatever vertical space is left over, shrinking to nothing rather than panicking
/// when the terminal is too small to fit everything - see [`body_layout`] for how it's further
/// split into the tree/sidebar/display sub-regions.
pub fn dashboard_layout(area: Rect) -> DashboardLayout {
    let [header, body, log, command_bar] = Layout::vertical([
        Constraint::Length(HEADER_HEIGHT),
        Constraint::Min(0),
        Constraint::Length(LOG_HEIGHT),
        Constraint::Length(COMMAND_BAR_HEIGHT),
    ])
    .areas(area);

    DashboardLayout {
        header,
        body,
        log,
        command_bar,
    }
}

/// Splits `body` into the display strip (only when `has_display`), the EVSE/connector tree, and,
/// once `body` is at least [`SIDEBAR_MIN_BODY_WIDTH`] columns wide, a detail sidebar for the
/// focused connector. Narrower than that, `sidebar` is `None` and the tree takes the full width;
/// the caller renders the same detail inline instead (see [`render`]).
pub fn body_layout(body: Rect, has_display: bool) -> BodyLayout {
    let display_height = if has_display { DISPLAY_HEIGHT } else { 0 };
    let [display, rest] =
        Layout::vertical([Constraint::Length(display_height), Constraint::Min(0)]).areas(body);

    if rest.width >= SIDEBAR_MIN_BODY_WIDTH {
        let [tree, sidebar] =
            Layout::horizontal([Constraint::Min(0), Constraint::Length(SIDEBAR_WIDTH)]).areas(rest);
        BodyLayout {
            display,
            tree,
            sidebar: Some(sidebar),
        }
    } else {
        BodyLayout {
            display,
            tree: rest,
            sidebar: None,
        }
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

/// Animation frames for the header's heartbeat pulse - see [`heartbeat_pulse_frame`]. A dim
/// dot growing to a bright, filled circle and back, distinct from [`SPINNER_FRAMES`] both in
/// shape (breathing, not rotating) and cadence, so the two are never mistaken for each other
/// when a connect attempt happens to start right as the pulse is mid-cycle.
const HEARTBEAT_FRAMES: [&str; 4] = ["·", "○", "●", "○"];

/// How long each heartbeat frame is shown, in milliseconds. Slower than the connecting spinner
/// (a full breathe takes 1.2s) because this runs *all the time* the dashboard is on screen,
/// not just during a brief connect attempt - a faster cadence would be distracting rather than
/// reassuring.
const HEARTBEAT_FRAME_MS: u128 = 300;

/// Picks the header's "alive and ticking" pulse frame for `uptime`.
///
/// This deliberately does not claim to reflect real OCPP Heartbeat traffic:
/// `ChargerState` exposes no sent/received message counts (see the roadmap's "No OCPP message
/// counters" gap), so a pulse tied to protocol heartbeats would either be fake or need upstream
/// plumbing that doesn't exist yet. What *is* honestly available is `uptime` itself, which only
/// advances when [`ChargerState::tick`] actually runs - so a pulse driven by it proves the
/// simulation loop is alive, which is exactly what distinguishes "idle" from "hung". Same
/// determinism rule as [`connecting_spinner_frame`]: simulated `uptime`, never a wall clock, so
/// goldens built from a fixed `elapsed` stay reproducible.
fn heartbeat_pulse_frame(uptime: Duration) -> &'static str {
    let index = (uptime.as_millis() / HEARTBEAT_FRAME_MS) as usize % HEARTBEAT_FRAMES.len();
    HEARTBEAT_FRAMES[index]
}

/// Compact uptime/duration formatting: `"0s"` while brand new, `"42s"` under a minute,
/// `"4m 12s"` under an hour, `"1h 04m"` from there on. Each tier only carries the units a user
/// actually needs at that scale - nobody needs "0h" prefixed on "4m 12s", or seconds once a
/// session has run for hours. Used for both the header's charger uptime and the sidebar's
/// per-connector session duration.
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
            (
                theme::glyph_field(connecting_spinner_frame(state.uptime)),
                connecting_style,
            ),
            ("connecting...".to_string(), connecting_style),
        ]
    } else {
        vec![
            (
                theme::glyph_field(theme::connection_glyph(state.connection_status)),
                theme::connection_style(state.connection_status),
            ),
            (
                state.connection_status.to_string(),
                theme::connection_style(state.connection_status),
            ),
        ]
    };

    let mut base = status;
    base.push(("  |  ".to_string(), theme::text_dim()));
    base.push((state.config.id.clone(), theme::text()));
    base.push(("  |  ".to_string(), theme::text_dim()));
    base.push((state.config.ocpp_version.to_string(), theme::text_dim()));

    let mode_segment = vec![
        ("  |  ".to_string(), theme::text_dim()),
        (mode_text(state), theme::text()),
    ];

    // The heartbeat pulse rides along with uptime rather than getting its own priority tier:
    // it has no meaning on its own (it's just a moving dot) and only earns a place in the
    // header as proof that the uptime figure next to it is actually live, not frozen.
    let uptime_segment = vec![
        ("  |  up ".to_string(), theme::text_dim()),
        (format_uptime(state.uptime), theme::text_dim()),
        ("  ".to_string(), theme::text_dim()),
        (
            theme::glyph_field(heartbeat_pulse_frame(state.uptime)),
            theme::text_dim(),
        ),
    ];

    // Widths are counted in `char`s, not bytes, matching `theme::glyph_field`'s convention -
    // every segment used here is either fixed-width ASCII or a `glyph_field` whose width is
    // already normalized to exactly two columns, so `chars().count()` and on-screen column
    // count agree.
    let width_of = |segments: &[(String, Style)]| -> usize {
        segments.iter().map(|(text, _)| text.chars().count()).sum()
    };

    let with_mode: Vec<(String, Style)> = base.iter().cloned().chain(mode_segment).collect();
    let with_mode_and_uptime: Vec<(String, Style)> =
        with_mode.iter().cloned().chain(uptime_segment).collect();

    if width_of(&with_mode_and_uptime) <= width {
        with_mode_and_uptime
    } else if width_of(&with_mode) <= width {
        with_mode
    } else {
        base
    }
}

/// The "most notable" status across an EVSE's connectors, used for the tree's per-EVSE summary
/// row - a purely presentational rollup (there's no EVSE-level status in `charge_point_simulator_core`,
/// nor should there be: an EVSE is just its connectors). Ordered so the state a user most wants
/// to notice at a glance wins: a fault anywhere on the EVSE outranks everything else, then an
/// active charging session, then merely occupied/reserved/unavailable, with a fully idle EVSE
/// (or one with no connectors at all) reading as `Available`.
fn evse_summary_status(evse: &EvseState) -> ConnectorStatus {
    const PRIORITY: [ConnectorStatus; 5] = [
        ConnectorStatus::Faulted,
        ConnectorStatus::Charging,
        ConnectorStatus::Occupied,
        ConnectorStatus::Reserved,
        ConnectorStatus::Unavailable,
    ];
    PRIORITY
        .into_iter()
        .find(|&status| {
            evse.connectors
                .iter()
                .any(|connector| connector.status == status)
        })
        .unwrap_or(ConnectorStatus::Available)
}

/// A 10-cell "state of charge" bar (`"███░░░░░░░"` at 34%), filled left to right. Rounds to the
/// nearest cell rather than flooring so, e.g., 95% reads as visibly closer to full than 91%
/// would at the same resolution.
fn soc_bar(state_of_charge: f64) -> String {
    const CELLS: usize = 10;
    let filled = ((state_of_charge.clamp(0.0, 100.0) / 100.0) * CELLS as f64).round() as usize;
    format!("{}{}", "█".repeat(filled), "░".repeat(CELLS - filled))
}

/// One EVSE's summary row in the tree: `EVSE 1  ● charging   7.40 kW   32.2 A   1.233 kWh`.
fn evse_summary_line(evse: &EvseState) -> Line<'static> {
    let status = evse_summary_status(evse);
    Line::from(vec![
        Span::styled(format!("EVSE {}", evse.id), theme::text()),
        Span::raw("  "),
        Span::styled(
            theme::glyph_field(theme::connector_glyph(status)),
            theme::connector_style(status),
        ),
        Span::styled(status.to_string(), theme::connector_style(status)),
        Span::raw("   "),
        Span::styled(format!("{:.2}", evse.metrics.power_kw), theme::text()),
        Span::styled(" kW  ", theme::text_dim()),
        Span::styled(format!("{:.1}", evse.metrics.current_a), theme::text()),
        Span::styled(" A  ", theme::text_dim()),
        Span::styled(format!("{:.3}", evse.metrics.energy_kwh), theme::text()),
        Span::styled(" kWh", theme::text_dim()),
    ])
}

/// One connector's row in the tree, indented beneath its EVSE:
/// `  ▸ C1  ● charging   Tesla-M3   34%`. The focused connector gets the `▸` marker and its
/// whole row painted with `theme::selected()` (rather than just the marker) so it reads as
/// unmistakably focused even without color, the same way the command palette highlights its
/// selected row.
fn connector_line(connector: &ConnectorState, focused: bool) -> Line<'static> {
    let marker = if focused { "▸ " } else { "  " };
    let row_style = if focused {
        theme::selected()
    } else {
        theme::connector_style(connector.status)
    };
    let label_style = if focused {
        theme::selected()
    } else {
        theme::text()
    };

    let mut spans = vec![
        Span::raw(format!("  {marker}")),
        Span::styled(format!("C{}", connector.id), label_style),
        Span::raw("  "),
        Span::styled(
            theme::glyph_field(theme::connector_glyph(connector.status)),
            row_style,
        ),
        Span::styled(connector.status.to_string(), row_style),
    ];

    if let Some(vehicle) = &connector.vehicle {
        let vehicle_style = if focused {
            theme::selected()
        } else {
            theme::text()
        };
        spans.push(Span::raw("   "));
        spans.push(Span::styled(vehicle.id.clone(), vehicle_style));
        if let Some(soc) = vehicle.state_of_charge {
            let soc_style = if focused {
                theme::selected()
            } else {
                theme::text_dim()
            };
            spans.push(Span::raw("   "));
            spans.push(Span::styled(format!("{:.0}%", soc), soc_style));
        }
    }

    Line::from(spans)
}

/// A single condensed detail line for the focused connector, rendered directly beneath its row
/// in the tree when the terminal is too narrow for the sidebar (see [`body_layout`]) - the same
/// facts the sidebar shows (vehicle, state of charge, session duration, EVSE metrics), just
/// folded onto one line since a narrow terminal is often a short one too.
fn inline_detail_line(evse: &EvseState, connector: &ConnectorState) -> Line<'static> {
    let mut spans = vec![Span::raw("        ")];
    match &connector.vehicle {
        Some(vehicle) => {
            spans.push(Span::styled(vehicle.id.clone(), theme::text()));
            if let Some(soc) = vehicle.state_of_charge {
                spans.push(Span::raw("  "));
                spans.push(Span::styled(
                    format!("{:.0}% {}", soc, soc_bar(soc)),
                    theme::text_dim(),
                ));
            }
            spans.push(Span::raw("  "));
        }
        None => {
            spans.push(Span::styled("no vehicle  ", theme::text_muted()));
        }
    }
    spans.push(Span::styled("session ", theme::text_dim()));
    spans.push(Span::styled(
        format_uptime(connector.session_duration),
        theme::text(),
    ));
    spans.push(Span::raw("  "));
    spans.push(Span::styled(
        format!("{:.2} kW", evse.metrics.power_kw),
        theme::text_dim(),
    ));
    Line::from(spans)
}

/// The full detail sidebar for the focused connector: status, vehicle, state of charge with a
/// small bar, session duration, and the parent EVSE's power/current/energy.
fn sidebar_lines(evse: &EvseState, connector: &ConnectorState) -> Vec<Line<'static>> {
    let mut lines = vec![
        Line::from(vec![Span::styled(
            format!("EVSE {} / C{}", evse.id, connector.id),
            theme::text(),
        )]),
        Line::from(vec![
            Span::styled(
                theme::glyph_field(theme::connector_glyph(connector.status)),
                theme::connector_style(connector.status),
            ),
            Span::styled(
                connector.status.to_string(),
                theme::connector_style(connector.status),
            ),
        ]),
    ];

    match &connector.vehicle {
        Some(vehicle) => {
            lines.push(Line::from(vec![
                Span::styled("vehicle  ", theme::text_dim()),
                Span::styled(vehicle.id.clone(), theme::text()),
            ]));
            if let Some(soc) = vehicle.state_of_charge {
                lines.push(Line::from(vec![
                    Span::styled("soc      ", theme::text_dim()),
                    Span::styled(format!("{:.0}% ", soc), theme::text()),
                    Span::styled(soc_bar(soc), theme::accent()),
                ]));
            }
        }
        None => lines.push(Line::styled("no vehicle", theme::text_muted())),
    }

    lines.push(Line::from(vec![
        Span::styled("session  ", theme::text_dim()),
        Span::styled(format_uptime(connector.session_duration), theme::text()),
    ]));

    lines.push(Line::default());
    lines.push(Line::from(vec![
        Span::styled(format!("{:.2}", evse.metrics.power_kw), theme::text()),
        Span::styled(" kW  ", theme::text_dim()),
        Span::styled(format!("{:.1}", evse.metrics.current_a), theme::text()),
        Span::styled(" A  ", theme::text_dim()),
        Span::styled(format!("{:.3}", evse.metrics.energy_kwh), theme::text()),
        Span::styled(" kWh", theme::text_dim()),
    ]));

    lines
}

/// The whole tree: every EVSE with its connectors indented beneath it as children. When
/// `inline_detail` is set (no room for the sidebar - see [`body_layout`]), the focused
/// connector's detail is appended directly beneath its row via [`inline_detail_line`].
fn tree_lines(
    charger: &ChargerState,
    focused: FocusedConnector,
    inline_detail: bool,
) -> Vec<Line<'static>> {
    if charger.evses.is_empty() {
        return vec![Line::styled("no EVSEs configured", theme::text_muted())];
    }

    let mut lines = Vec::new();
    for (evse_index, evse) in charger.evses.iter().enumerate() {
        lines.push(evse_summary_line(evse));

        if evse.connectors.is_empty() {
            lines.push(Line::styled("    no connectors", theme::text_muted()));
            continue;
        }

        for (connector_index, connector) in evse.connectors.iter().enumerate() {
            let is_focused = focused.evse == evse_index && focused.connector == connector_index;
            lines.push(connector_line(connector, is_focused));
            if inline_detail && is_focused {
                lines.push(inline_detail_line(evse, connector));
            }
        }
    }
    lines
}

/// The index, within [`tree_lines`]'s output, of the connector row `focused` points at (i.e.
/// the row the tree should keep scrolled into view). Ignores inline-detail lines, since those
/// only ever appear *after* the focused row and so never affect how far it is from the top.
fn tree_focus_line_index(charger: &ChargerState, focused: FocusedConnector) -> usize {
    let mut index = 0;
    for (evse_index, evse) in charger.evses.iter().enumerate() {
        index += 1; // the EVSE summary line
        if evse.connectors.is_empty() {
            index += 1; // "no connectors"
            continue;
        }
        for connector_index in 0..evse.connectors.len() {
            if evse_index == focused.evse && connector_index == focused.connector {
                return index;
            }
            index += 1;
        }
    }
    0
}

/// How far the tree should be scrolled (lines cut from the top) so that `focus_line` stays
/// visible within a `height`-row window over `total_lines` lines. The tree has no independent
/// scroll key of its own - unlike the log pane - so this always tracks focus rather than
/// following a user-driven offset: it holds still while the focused row is already on screen,
/// and moves the minimum amount needed to bring it back into view when focus moves off-screen.
fn tree_scroll_offset(total_lines: usize, height: usize, focus_line: usize) -> usize {
    if height == 0 || total_lines <= height {
        return 0;
    }
    let max_offset = total_lines - height;
    if focus_line < height {
        0
    } else {
        (focus_line + 1 - height).min(max_offset)
    }
}

/// Renders a scrollbar along the right edge of `area`, one row down from the top (so it doesn't
/// overwrite a section's top-rule title). Shared by the tree and log panes so both scroll the
/// same way and look the same doing it.
fn render_scrollbar(frame: &mut Frame, area: Rect, max_offset: usize, position: usize) {
    let mut state = ScrollbarState::new(max_offset).position(position);
    let track = Rect {
        y: area.y + 1,
        height: area.height.saturating_sub(1),
        ..area
    };
    frame.render_stateful_widget(
        Scrollbar::new(ScrollbarOrientation::VerticalRight)
            .begin_symbol(None)
            .end_symbol(None),
        track,
        &mut state,
    );
}

/// The EVSE/connector pair `focused` points at, if it identifies a real connector in `charger`.
fn focused_connector(
    charger: &ChargerState,
    focused: FocusedConnector,
) -> Option<(&EvseState, &ConnectorState)> {
    let evse = charger.evses.get(focused.evse)?;
    let connector = evse.connectors.get(focused.connector)?;
    Some((evse, connector))
}

pub(super) fn render(frame: &mut Frame, view: &DashboardView) {
    let has_display = view.charger.is_some_and(|state| state.config.has_display);
    let layout = dashboard_layout(frame.area());
    let body = body_layout(layout.body, has_display);

    // `focused: false` at every section call site below: panel-level focus (as opposed to the
    // tree's own connector-focus concept, handled separately) has no representation in `App`
    // yet - that arrives in a later phase, at which point these become real.
    let focused = false;

    if has_display {
        let message = view
            .charger
            .and_then(|state| state.display_message.as_deref());
        let line = match message {
            Some(message) => Line::styled(message, theme::text()),
            None => Line::styled("(blank)", theme::text_muted()),
        };
        frame.render_widget(
            Paragraph::new(line).block(theme::section("Display", focused)),
            body.display,
        );
    }

    let header_line = match view.charger {
        Some(state) => {
            let segments = header_segments(state, view.connecting, layout.header.width as usize);
            Line::from(
                segments
                    .into_iter()
                    .map(|(text, style)| Span::styled(text, style))
                    .collect::<Vec<_>>(),
            )
        }
        None => Line::styled("no charger selected", theme::text_muted()),
    };
    frame.render_widget(
        Paragraph::new(header_line).block(theme::header(focused)),
        layout.header,
    );

    let tree_title = match view.charger {
        Some(state) if !state.evses.is_empty() => {
            format!("EVSEs ({}/{})", view.focused.evse + 1, state.evses.len())
        }
        _ => "EVSEs".to_string(),
    };
    let tree = match view.charger {
        Some(state) => tree_lines(state, view.focused, body.sidebar.is_none()),
        None => vec![Line::styled("no charger selected", theme::text_muted())],
    };
    // A `section` only spends 1 row on chrome (the top rule), matching `render_log_pane`.
    let tree_height = body.tree.height.saturating_sub(1) as usize;
    let tree_total = tree.len();
    let tree_needs_scrollbar = tree_total > tree_height && tree_height > 0;
    let tree_offset = view
        .charger
        .map(|state| {
            tree_scroll_offset(
                tree_total,
                tree_height,
                tree_focus_line_index(state, view.focused),
            )
        })
        .unwrap_or(0);

    let tree_block = theme::section(tree_title, focused);
    let mut tree_area = tree_block.inner(body.tree);
    if tree_needs_scrollbar {
        tree_area.width = tree_area.width.saturating_sub(1);
    }
    frame.render_widget(tree_block, body.tree);
    // Deliberately no `.wrap(...)`: `Wrap { trim: true }` strips leading whitespace from every
    // line, which would eat the tree's indentation (the connector rows' leading spaces, the
    // focus marker). A line that doesn't fit `body.tree`'s width is simply clipped at the
    // right edge instead of wrapping - the same trade `evse_detail` made before this panel
    // existed.
    let visible_tree: Vec<Line> = tree
        .into_iter()
        .skip(tree_offset)
        .take(tree_height.max(1))
        .collect();
    frame.render_widget(Paragraph::new(visible_tree), tree_area);
    if tree_needs_scrollbar {
        render_scrollbar(frame, body.tree, tree_total - tree_height, tree_offset);
    }

    if let Some(sidebar_area) = body.sidebar {
        let sidebar = match view
            .charger
            .and_then(|state| focused_connector(state, view.focused))
        {
            Some((evse, connector)) => sidebar_lines(evse, connector),
            None => vec![Line::styled("no connector focused", theme::text_muted())],
        };
        frame.render_widget(
            Paragraph::new(sidebar).block(theme::section("Detail", focused)),
            sidebar_area,
        );
    }

    render_log_pane(frame, view, layout.log, focused);

    let command_bar_line = match view.log_filter_input {
        // The filter prompt owns the command bar while it's open: it's a modal input, and
        // showing keybinding hints for keys that are currently being typed into it would lie.
        Some(input) => Line::from(vec![
            Span::styled("/", theme::accent()),
            Span::styled(input, theme::text()),
            Span::styled("▏", theme::accent()),
            Span::styled("  Enter: keep  Esc: clear", theme::text_dim()),
        ]),
        None => match view.status_message {
            Some((severity, message)) => {
                let style = match severity {
                    StatusSeverity::Ok => theme::ok(),
                    StatusSeverity::Error => theme::error(),
                };
                Line::styled(message, style)
            }
            // Derived from the keybinding table rather than spelled out here, so the hint
            // can't drift from what the keys actually do - see `crate::keybindings`. Shortened
            // by priority to fit the command bar's actual width, the same way `header_segments`
            // drops its own lowest-priority segments first, rather than truncating mid-word.
            None => Line::styled(
                crate::keybindings::dashboard_hint_for_width(layout.command_bar.width as usize),
                theme::text_dim(),
            ),
        },
    };
    frame.render_widget(Paragraph::new(command_bar_line), layout.command_bar);
}

/// The log pane: a protocol trace rather than a wall of pre-formatted strings. Each entry is
/// rendered as columns - timestamp, level, direction, elided target, message - so the eye can
/// find one column without reading the others (see the "density over decoration" principle).
fn render_log_pane(frame: &mut Frame, view: &DashboardView, area: Rect, focused: bool) {
    let logs = view.logs;
    // A `section` only spends 1 row on chrome (the top rule), not 2, so all but 1 row of
    // `area`'s height is available for content.
    let log_height = area.height.saturating_sub(1) as usize;

    // The title carries the pane's whole state - filter, level threshold, and whether the view
    // is following the tail - because none of those are visible from the lines themselves.
    let mut title = String::from("Logs");
    if let Some(filter) = logs.filter() {
        title.push_str(&format!(" /{filter}"));
    }
    if logs.level_threshold() != LogLevel::Info {
        title.push_str(&format!(" ≥{}", level_label(logs.level_threshold())));
    }
    if logs.is_paused() {
        title.push_str(" (paused)");
    }

    // A real scrollbar, so "where am I in the buffer" doesn't have to be inferred from the
    // title alone. Only worth drawing when there's more to see than fits.
    let total = logs.filtered_len();
    let needs_scrollbar = total > log_height && log_height > 0;

    // The block is drawn on the full area (so its top rule still spans the pane), but the text
    // gives up the rightmost column when a scrollbar is present - otherwise the thumb
    // overwrites the tail of every long message.
    let block = theme::section(title, focused);
    let mut text_area = block.inner(area);
    if needs_scrollbar {
        text_area.width = text_area.width.saturating_sub(1);
    }
    frame.render_widget(block, area);
    let lines: Vec<Line> = logs
        .visible_lines(log_height)
        .into_iter()
        .map(log_line)
        .collect();
    frame.render_widget(Paragraph::new(lines), text_area);

    if needs_scrollbar {
        render_scrollbar(
            frame,
            area,
            total.saturating_sub(log_height),
            total
                .saturating_sub(log_height)
                .saturating_sub(logs.scroll_offset()),
        );
    }
}

fn level_label(level: LogLevel) -> &'static str {
    match level {
        LogLevel::Error => "ERROR",
        LogLevel::Warn => "WARN",
        LogLevel::Info => "INFO",
        LogLevel::Debug => "DEBUG",
        LogLevel::Trace => "TRACE",
    }
}

/// One log entry as aligned columns. Entries the TUI generates itself have no timestamp; their
/// column is left blank rather than filled with a wall-clock reading they didn't come from.
fn log_line(entry: &crate::logs::LogEntry) -> Line<'_> {
    let mut spans = vec![
        Span::styled(
            format!("{:<12} ", entry.timestamp.as_deref().unwrap_or("")),
            theme::text_muted(),
        ),
        Span::styled(
            format!("{:<5} ", level_label(entry.level)),
            theme::log_level(entry.level),
        ),
        Span::styled(
            match entry.direction {
                Some(Direction::Outbound) => "→ ",
                Some(Direction::Inbound) => "← ",
                None => "  ",
            },
            theme::accent(),
        ),
        Span::styled(format!("{:<14} ", entry.short_target()), theme::text_dim()),
    ];
    if let Some(action) = &entry.action {
        spans.push(Span::styled(format!("{action} "), theme::accent()));
    }
    spans.push(Span::styled(entry.message.as_str(), theme::text()));
    for (name, value) in &entry.fields {
        spans.push(Span::styled(format!(" {name}={value}"), theme::text_dim()));
    }
    Line::from(spans)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn area(width: u16, height: u16) -> Rect {
        Rect::new(0, 0, width, height)
    }

    // --- dashboard_layout: header / body / log / command bar -------------------------------

    #[test]
    fn stacks_regions_top_to_bottom_in_order() {
        let layout = dashboard_layout(area(120, 40));

        assert_eq!(layout.header.y, 0);
        assert_eq!(layout.body.y, layout.header.bottom());
        assert_eq!(layout.log.y, layout.body.bottom());
        assert_eq!(layout.command_bar.y, layout.log.bottom());
        assert_eq!(layout.command_bar.bottom(), 40);
    }

    #[test]
    fn fixed_regions_use_their_configured_height_when_space_allows() {
        let layout = dashboard_layout(area(120, 40));

        assert_eq!(layout.header.height, HEADER_HEIGHT);
        assert_eq!(layout.log.height, LOG_HEIGHT);
        assert_eq!(layout.command_bar.height, COMMAND_BAR_HEIGHT);
    }

    #[test]
    fn body_absorbs_the_remaining_space() {
        let layout = dashboard_layout(area(120, 40));
        let fixed_height = HEADER_HEIGHT + LOG_HEIGHT + COMMAND_BAR_HEIGHT;

        assert_eq!(layout.body.height, 40 - fixed_height);
    }

    #[test]
    fn regions_span_the_full_width() {
        let layout = dashboard_layout(area(120, 40));

        for region in [layout.header, layout.body, layout.log, layout.command_bar] {
            assert_eq!(region.width, 120);
        }
    }

    #[test]
    fn shrinks_gracefully_when_the_terminal_is_too_small() {
        let layout = dashboard_layout(area(80, 2));

        assert_eq!(layout.body.height, 0);
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

    // --- body_layout: display / tree / sidebar ----------------------------------------------

    #[test]
    fn body_splits_tree_and_sidebar_horizontally_when_wide_enough() {
        let body = body_layout(area(120, 30), false);

        let sidebar = body.sidebar.expect("120 columns should fit a sidebar");
        assert_eq!(body.tree.x, 0);
        assert_eq!(sidebar.x, body.tree.right());
        assert_eq!(sidebar.width, SIDEBAR_WIDTH);
        assert_eq!(body.tree.width + sidebar.width, 120);
    }

    #[test]
    fn body_collapses_the_sidebar_below_the_width_threshold() {
        let body = body_layout(area(80, 30), false);

        assert!(body.sidebar.is_none());
        assert_eq!(body.tree.width, 80);
    }

    #[test]
    fn the_threshold_itself_still_fits_a_sidebar() {
        let body = body_layout(area(SIDEBAR_MIN_BODY_WIDTH, 30), false);
        assert!(body.sidebar.is_some());
    }

    #[test]
    fn one_column_below_the_threshold_collapses() {
        let body = body_layout(area(SIDEBAR_MIN_BODY_WIDTH - 1, 30), false);
        assert!(body.sidebar.is_none());
    }

    #[test]
    fn the_display_strip_takes_no_space_when_the_charger_has_no_display() {
        let body = body_layout(area(120, 30), false);

        assert_eq!(body.display.height, 0);
        assert_eq!(body.tree.y, body.display.y);
    }

    #[test]
    fn the_display_strip_reserves_its_configured_height_when_present() {
        let body = body_layout(area(120, 30), true);

        assert_eq!(body.display.height, DISPLAY_HEIGHT);
        assert_eq!(body.tree.y, body.display.bottom());
    }

    #[test]
    fn body_layout_does_not_panic_on_a_tiny_area() {
        let body = body_layout(area(80, 0), true);
        assert_eq!(body.tree.height, 0);
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

    // --- heartbeat_pulse_frame -----------------------------------------------------------

    #[test]
    fn heartbeat_pulse_frame_cycles_through_frames_as_simulated_uptime_advances() {
        assert_eq!(heartbeat_pulse_frame(Duration::ZERO), "\u{b7}");
        assert_eq!(
            heartbeat_pulse_frame(Duration::from_millis(300)),
            "\u{25cb}"
        );
        assert_eq!(
            heartbeat_pulse_frame(Duration::from_millis(600)),
            "\u{25cf}"
        );
        assert_eq!(
            heartbeat_pulse_frame(Duration::from_millis(900)),
            "\u{25cb}"
        );
    }

    #[test]
    fn heartbeat_pulse_frame_wraps_around_after_the_last_frame() {
        assert_eq!(heartbeat_pulse_frame(Duration::from_millis(1200)), "\u{b7}");
    }

    #[test]
    fn heartbeat_pulse_frame_changes_when_accumulated_at_the_apps_real_100ms_tick_cadence() {
        // Regression guard for the roadmap's "accumulators coarser than their increment"
        // trap: drive `uptime` forward in the same 100ms steps `App`'s input-poll loop
        // actually uses (see `INPUT_POLL_INTERVAL`), rather than jumping straight to
        // hand-picked totals, so a bug that only shows up when frames accumulate one small
        // step at a time can't hide behind a test that skips straight to the answer.
        const TICK: Duration = Duration::from_millis(100);
        let mut uptime = Duration::ZERO;
        let mut frames = Vec::new();
        for _ in 0..12 {
            let frame = heartbeat_pulse_frame(uptime);
            if frames.last() != Some(&frame) {
                frames.push(frame);
            }
            uptime += TICK;
        }
        // Over 1.2s (12 ticks of 100ms) at a 300ms frame period, the pulse must actually
        // change - a coarse accumulator that rounded 100ms increments away would show a
        // single static frame here even though `uptime` is visibly advancing.
        assert_eq!(frames, vec!["\u{b7}", "\u{25cb}", "\u{25cf}", "\u{25cb}"]);
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
        state.mode = SimulationMode::LiveCsms {
            url: "wss://csms.example.com".to_string(),
        };
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
    fn header_segments_show_the_heartbeat_pulse_alongside_uptime_when_there_is_room() {
        let mut state = charger_state_for_header("CP-CHARGE");
        state.uptime = Duration::from_millis(600);
        let text = segments_text(&header_segments(&state, false, 120));

        assert!(text.contains("up 0s"));
        assert!(text.contains(heartbeat_pulse_frame(state.uptime)));
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

        let mode_only_width =
            base_width + "  |  ".chars().count() + mode_text(&state).chars().count();
        let with_uptime_width = mode_only_width
            + "  |  up ".chars().count()
            + format_uptime(state.uptime).chars().count()
            + "  ".chars().count()
            + theme::glyph_field(heartbeat_pulse_frame(state.uptime))
                .chars()
                .count();

        let everything = segments_text(&header_segments(&state, false, with_uptime_width));
        assert!(everything.contains("local simulation"));
        assert!(everything.contains("up 4m 12s"));

        let mode_only = segments_text(&header_segments(&state, false, with_uptime_width - 1));
        assert!(
            mode_only.contains("local simulation"),
            "mode should still fit: {mode_only:?}"
        );
        assert!(
            !mode_only.contains("up 4m 12s"),
            "uptime should have been dropped: {mode_only:?}"
        );

        let floor_only = segments_text(&header_segments(&state, false, mode_only_width - 1));
        assert!(
            !floor_only.contains("local simulation"),
            "mode should have been dropped: {floor_only:?}"
        );
        assert!(
            floor_only.contains("CP-CHARGE"),
            "the id/status floor must never be dropped: {floor_only:?}"
        );
    }

    // --- evse_summary_status -------------------------------------------------------------

    fn evse_with_statuses(statuses: &[ConnectorStatus]) -> EvseState {
        EvseState {
            id: 1,
            connectors: statuses
                .iter()
                .enumerate()
                .map(|(index, &status)| ConnectorState {
                    id: index as u32 + 1,
                    status,
                    vehicle: None,
                    session_duration: Duration::ZERO,
                })
                .collect(),
            metrics: Default::default(),
        }
    }

    #[test]
    fn evse_summary_status_is_available_when_every_connector_is() {
        let evse = evse_with_statuses(&[ConnectorStatus::Available, ConnectorStatus::Available]);
        assert_eq!(evse_summary_status(&evse), ConnectorStatus::Available);
    }

    #[test]
    fn evse_summary_status_is_available_with_no_connectors() {
        let evse = evse_with_statuses(&[]);
        assert_eq!(evse_summary_status(&evse), ConnectorStatus::Available);
    }

    #[test]
    fn evse_summary_status_prefers_charging_over_merely_occupied() {
        let evse = evse_with_statuses(&[ConnectorStatus::Occupied, ConnectorStatus::Charging]);
        assert_eq!(evse_summary_status(&evse), ConnectorStatus::Charging);
    }

    #[test]
    fn evse_summary_status_prefers_a_fault_over_everything_else() {
        let evse = evse_with_statuses(&[ConnectorStatus::Charging, ConnectorStatus::Faulted]);
        assert_eq!(evse_summary_status(&evse), ConnectorStatus::Faulted);
    }

    // --- soc_bar ---------------------------------------------------------------------------

    #[test]
    fn soc_bar_is_empty_at_zero_and_full_at_a_hundred() {
        assert_eq!(soc_bar(0.0), "░░░░░░░░░░");
        assert_eq!(soc_bar(100.0), "██████████");
    }

    #[test]
    fn soc_bar_rounds_to_the_nearest_cell() {
        assert_eq!(soc_bar(34.0), "███░░░░░░░");
    }

    // --- tree scrolling ----------------------------------------------------------------

    fn charger_with_evses(evses: Vec<EvseState>) -> ChargerState {
        let mut state = charger_state_for_header("CP-CHARGE");
        state.evses = evses;
        state
    }

    #[test]
    fn tree_focus_line_index_finds_the_focused_connectors_row() {
        let charger = charger_with_evses(vec![
            evse_with_statuses(&[ConnectorStatus::Available, ConnectorStatus::Available]),
            evse_with_statuses(&[ConnectorStatus::Available]),
        ]);

        // EVSE 1 summary (0), connector 1 (1), connector 2 (2), EVSE 2 summary (3), connector 1 (4)
        assert_eq!(
            tree_focus_line_index(
                &charger,
                FocusedConnector {
                    evse: 0,
                    connector: 1
                }
            ),
            2
        );
        assert_eq!(
            tree_focus_line_index(
                &charger,
                FocusedConnector {
                    evse: 1,
                    connector: 0
                }
            ),
            4
        );
    }

    #[test]
    fn tree_scroll_offset_stays_at_zero_while_focus_is_already_visible() {
        assert_eq!(tree_scroll_offset(20, 5, 0), 0);
        assert_eq!(tree_scroll_offset(20, 5, 4), 0);
    }

    #[test]
    fn tree_scroll_offset_moves_the_minimum_amount_to_reveal_focus_below_the_window() {
        // 20 lines, a 5-row window, focus on line 10 (0-indexed): the window must end exactly
        // on the focused line, i.e. start at 10 - 5 + 1 = 6.
        assert_eq!(tree_scroll_offset(20, 5, 10), 6);
    }

    #[test]
    fn tree_scroll_offset_never_exceeds_the_maximum_offset() {
        // Focus on the very last line still clamps to `total - height`, not further.
        assert_eq!(tree_scroll_offset(20, 5, 19), 15);
    }

    #[test]
    fn tree_scroll_offset_is_zero_when_everything_fits() {
        assert_eq!(tree_scroll_offset(3, 5, 2), 0);
        assert_eq!(tree_scroll_offset(0, 5, 0), 0);
    }
}
