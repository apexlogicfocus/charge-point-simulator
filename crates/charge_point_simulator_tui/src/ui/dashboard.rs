use std::time::Duration;

use ratatui::Frame;
use ratatui::layout::{Constraint, Layout, Rect};
use ratatui::style::Style;
use ratatui::text::{Line, Span};
use ratatui::widgets::{Paragraph, Scrollbar, ScrollbarOrientation, ScrollbarState};

use super::view::DashboardView;
use crate::app::{CampaignProgress, FocusedConnector, StatusSeverity};
use crate::logs::{Direction, LogLevel};
use crate::theme;
use charge_point_simulator_core::charger::{
    ChargerConfig, ChargerState, ConnectionStatus, ConnectorState, ConnectorStatus, EvseState,
    FirmwareInstallStage, InFlightTransfer, PowerHistory, SimulationMode,
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
    /// Zero height unless the last CSMS connection attempt failed - see [`BodyStrips::connection`].
    /// First of the strips, and taller than the others: it is the only one that reports something
    /// broken, and it has to carry the reason as well as what to do about it.
    pub connection: Rect,
    /// Zero height when the charger has no display - see [`BodyStrips::display`].
    pub display: Rect,
    /// Zero height for a charger that declares no capabilities at all - see
    /// [`BodyStrips::capabilities`] and [`capability_labels`].
    pub capabilities: Rect,
    /// Zero height unless a firmware campaign or file transfer is actually in flight - see
    /// [`body_layout`]'s `campaigns_active` and [`crate::app::CampaignProgress::is_active`]. A strip
    /// that only appears while something is happening costs the tree no rows the rest of the time,
    /// which is most of the time.
    pub campaigns: Rect,
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
/// Same shape as [`DISPLAY_HEIGHT`] - one row of content under a section rule - and, like the
/// display strip, only allotted at all when there is something to put in it.
const CAMPAIGNS_HEIGHT: u16 = 2;
/// Likewise: one row listing what the charger declares, under its own rule.
const CAPABILITIES_HEIGHT: u16 = 2;
/// Three rows, not two: a rule, the reason the connection failed, and what to do about it. The extra
/// row is the point - an error with no next step leaves the user reading a dead end.
const CONNECTION_HEIGHT: u16 = 3;
const LOG_HEIGHT: u16 = 11;
const COMMAND_BAR_HEIGHT: u16 = 1;

/// The detail sidebar's fixed width when there's room for one.
const SIDEBAR_WIDTH: u16 = 32;

/// How many power samples the sidebar's sparkline shows - one cell each, sized to leave room for the
/// `Ns` window label beside it inside [`SIDEBAR_WIDTH`]. `core`'s `PowerHistory` keeps more
/// (`PowerHistory::CAPACITY`); this is how much of it fits here.
const SIDEBAR_SPARKLINE_CELLS: usize = 24;

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

/// Which of the body's optional strips this frame needs. Each one costs the tree rows for as long
/// as it's on screen, so each is allotted only when it has something to say - a charger with no
/// display, no declared capabilities and nothing installing spends nothing on any of them, which is
/// every charger most of the time.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct BodyStrips {
    /// The last CSMS connection attempt failed and hasn't been retried - see
    /// `App::connection_failure`.
    pub connection: bool,
    /// The charger has a display (`ChargerConfig::has_display`), so there is a message - or a
    /// deliberate `(blank)` - to show.
    pub display: bool,
    /// The charger declares at least one capability, so there is something to list.
    pub capabilities: bool,
    /// A firmware campaign or file transfer is in flight - see
    /// [`crate::app::CampaignProgress::is_active`].
    pub campaigns: bool,
}

/// Which optional strips a frame showing `charger` with `campaigns` in flight needs - the single
/// place that decision is made, so [`render`] and [`crate::app::App::handle_mouse_event`]'s
/// hit-testing can never lay the body out differently for the same state.
pub(crate) fn body_strips_for(
    charger: Option<&ChargerState>,
    campaigns: &CampaignProgress,
    connection_failure: Option<&str>,
) -> BodyStrips {
    BodyStrips {
        connection: connection_failure.is_some(),
        display: charger.is_some_and(|state| state.config.has_display),
        capabilities: charger.is_some_and(|state| !capability_labels(&state.config).is_empty()),
        campaigns: campaigns.is_active(),
    }
}

/// Splits `body` into whichever of the optional strips `strips` calls for, the EVSE/connector tree,
/// and, once `body` is at least [`SIDEBAR_MIN_BODY_WIDTH`] columns wide, a detail sidebar for the
/// focused connector. Narrower than that, `sidebar` is `None` and the tree takes the full width; the
/// caller renders the same detail inline instead (see [`render`]).
pub fn body_layout(body: Rect, strips: BodyStrips) -> BodyLayout {
    let height = |wanted: bool, height: u16| if wanted { height } else { 0 };
    let [connection, display, capabilities, campaigns, rest] = Layout::vertical([
        Constraint::Length(height(strips.connection, CONNECTION_HEIGHT)),
        Constraint::Length(height(strips.display, DISPLAY_HEIGHT)),
        Constraint::Length(height(strips.capabilities, CAPABILITIES_HEIGHT)),
        Constraint::Length(height(strips.campaigns, CAMPAIGNS_HEIGHT)),
        Constraint::Min(0),
    ])
    .areas(body);

    if rest.width >= SIDEBAR_MIN_BODY_WIDTH {
        let [tree, sidebar] =
            Layout::horizontal([Constraint::Min(0), Constraint::Length(SIDEBAR_WIDTH)]).areas(rest);
        BodyLayout {
            connection,
            display,
            capabilities,
            campaigns,
            tree,
            sidebar: Some(sidebar),
        }
    } else {
        BodyLayout {
            connection,
            display,
            capabilities,
            campaigns,
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

/// What the header's status slot should say about the CSMS link, which is not always what
/// `ChargerState::connection_status` holds.
///
/// [`Self::Failed`] exists because a charger whose dial failed keeps whatever status it was seeded
/// with - `Booting` - forever: the connection thread exited, so no snapshot will ever arrive to
/// correct it, and `apply_ocpp_state` is the only thing that writes that field. Rendering it would
/// mean showing "booting" for a charger that is not booting and never will be, which is the exact
/// "truth in the status bar" failure this enum exists to prevent.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum LinkState {
    /// A connection attempt is in flight (`App::connect_result_receiver` is `Some`).
    Connecting,
    /// The last attempt failed and nothing has been done about it yet - see
    /// `App::connection_failure`.
    Failed,
    /// Whatever the charger itself reports. The normal case, including a local charger's `Offline`.
    Reported,
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
fn header_segments(state: &ChargerState, link: LinkState, width: usize) -> Vec<(String, Style)> {
    // While a connection attempt is pending, an animated spinner (driven by simulated
    // `uptime`, never wall-clock time - see `connecting_spinner_frame`) replaces the normal
    // status glyph: `connection_status` itself is still `Booting` at this point (the OCPP
    // bridge hasn't reported anything yet), so without this the header would sit static and
    // give no feedback that anything is happening.
    let status: Vec<(String, Style)> = match link {
        LinkState::Connecting => {
            let connecting_style = theme::connection_style(ConnectionStatus::Booting);
            vec![
                (
                    theme::glyph_field(connecting_spinner_frame(state.uptime)),
                    connecting_style,
                ),
                ("connecting...".to_string(), connecting_style),
            ]
        }
        // Never `state.connection_status` here - see `LinkState::Failed`. The `✕` glyph is the one
        // `Offline` uses, since both mean "there is no link", and the error color plus the word
        // separate a dial that failed from a charger that never tried.
        LinkState::Failed => vec![
            (theme::glyph_field("✕"), theme::error()),
            ("connection failed".to_string(), theme::error()),
        ],
        LinkState::Reported => vec![
            (
                theme::glyph_field(theme::connection_glyph(state.connection_status)),
                theme::connection_style(state.connection_status),
            ),
            (
                state.connection_status.to_string(),
                theme::connection_style(state.connection_status),
            ),
        ],
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
///
/// `Occupied` deliberately outranks `Reserved`: a car physically present is the more actionable
/// fact than a booking against a connector that may still be empty. Settled decision, not an
/// oversight - see `docs/tui-roadmap.md`.
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

/// Whether the connector's cable lock actuator reports engaged - the hardware fact, spelled the
/// way a technician would read it off a real charger rather than as a bare `true`/`false`. Always
/// one of two words, never blank: "not locked" is a real, knowable state, not a missing reading.
fn lock_text(locked: bool) -> &'static str {
    if locked { "engaged" } else { "released" }
}

/// Whether the connector's contactor reports closed, i.e. whether energy can flow at all. Same
/// two-word rule as [`lock_text`].
fn contactor_text(contactor_closed: bool) -> &'static str {
    if contactor_closed { "closed" } else { "open" }
}

/// The current limit last applied to a connector, in amps.
///
/// The three cases `ConnectorState::current_limit_ma` deliberately keeps distinct stay distinct
/// here, because they mean genuinely different things to anyone testing smart charging: `None` is
/// "no profile limits this connector", `Some(0)` is "a profile suspended it" (which reads as
/// `0.0 A` on top of the word, so a limit that happens to *be* zero can't be mistaken for the
/// absence of one), and anything else is the limit itself.
fn current_limit_text(current_limit_ma: Option<u32>) -> String {
    match current_limit_ma {
        None => "none".to_string(),
        Some(0) => "suspended (0.0 A)".to_string(),
        Some(milliamps) => format!("{:.1} A", milliamps as f64 / 1000.0),
    }
}

/// Cumulative exported energy in kWh, from the Wh register the hardware keeps
/// (`ConnectorState::exported_energy_wh`). Three decimals, matching how `EvseMetrics::energy_kwh`
/// is rendered everywhere else, so the import and export figures read as the same kind of number.
fn exported_energy_text(exported_energy_wh: i64) -> String {
    format!("{:.3} kWh", exported_energy_wh as f64 / 1000.0)
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

    // Two of the four hardware facts (H7/H14) the sidebar spells out as labelled rows, and only
    // when they aren't the default. This line already runs to ~66 columns on a vehicle mid-session
    // and has to survive at 80, which is the width the narrow layout exists for - so it carries the
    // two that change what a CSMS developer does next (a profile is limiting this connector; it is
    // exporting rather than importing) and leaves lock and contactor to the sidebar, the same way
    // it already leaves current and energy there. An absent token means the default - unlimited,
    // importing - never an unknown reading.
    if let Some(milliamps) = connector.current_limit_ma {
        spans.push(Span::raw("  "));
        spans.push(Span::styled(
            format!("{:.1} A", milliamps as f64 / 1000.0),
            theme::text_dim(),
        ));
    }
    if connector.discharging {
        spans.push(Span::raw("  "));
        spans.push(Span::styled("V2G", theme::accent()));
    }

    Line::from(spans)
}

/// The full detail sidebar for the focused connector: status, vehicle, state of charge with a
/// small bar, session duration, the hardware layer's lock/contactor/current-limit and power
/// direction (see [`lock_text`] and friends), and the parent EVSE's power/current/energy.
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

    // The hardware layer's own view of this connector (H7/H14), which no `ChargePointState`
    // carries: what the actuators report, then what the meter's export register holds. Shown
    // unconditionally rather than only when "interesting", because for anyone testing smart
    // charging or a remote unlock, "the contactor is open" and "no limit applies" are the answers
    // they came for as often as the opposite.
    lines.push(Line::default());
    lines.push(Line::from(vec![
        Span::styled("lock     ", theme::text_dim()),
        Span::styled(lock_text(connector.locked), theme::text()),
    ]));
    lines.push(Line::from(vec![
        Span::styled("contact  ", theme::text_dim()),
        Span::styled(contactor_text(connector.contactor_closed), theme::text()),
    ]));
    lines.push(Line::from(vec![
        Span::styled("limit    ", theme::text_dim()),
        Span::styled(
            current_limit_text(connector.current_limit_ma),
            theme::text(),
        ),
    ]));

    // Direction is only ever worth a row when it isn't the default: a connector importing is what
    // the power/current figures below already say. Exported energy, on the other hand, outlives the
    // discharge that produced it - it is a cumulative register - so it stays visible afterwards.
    if connector.discharging {
        lines.push(Line::from(vec![
            Span::styled("power    ", theme::text_dim()),
            Span::styled("exporting (V2G)", theme::accent()),
        ]));
    }
    if connector.exported_energy_wh != 0 {
        lines.push(Line::from(vec![
            Span::styled("exported ", theme::text_dim()),
            Span::styled(
                exported_energy_text(connector.exported_energy_wh),
                theme::text(),
            ),
        ]));
    }

    lines.push(Line::default());
    lines.push(Line::from(vec![
        Span::styled(format!("{:.2}", evse.metrics.power_kw), theme::text()),
        Span::styled(" kW  ", theme::text_dim()),
        Span::styled(format!("{:.1}", evse.metrics.current_a), theme::text()),
        Span::styled(" A  ", theme::text_dim()),
        Span::styled(format!("{:.3}", evse.metrics.energy_kwh), theme::text()),
        Span::styled(" kWh", theme::text_dim()),
    ]));

    // The last minute of that power reading, so the figure above reads as part of a trend rather
    // than an instant - nothing at all until there is real history to draw (see `power_sparkline`).
    // Only the most recent `SIDEBAR_SPARKLINE_CELLS` samples: the sidebar is 32 columns wide and a
    // spark wider than its own panel would be clipped mid-series with no sign it had been.
    let samples = evse.power_history.samples();
    if !samples.is_empty() {
        let recent = &samples[samples.len().saturating_sub(SIDEBAR_SPARKLINE_CELLS)..];
        lines.push(Line::from(vec![
            Span::styled(power_sparkline(recent), theme::accent()),
            Span::styled(
                format!(
                    "  {}s",
                    (PowerHistory::SAMPLE_INTERVAL * recent.len() as u32).as_secs()
                ),
                theme::text_dim(),
            ),
        ]));
    }

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

/// The reverse of [`tree_focus_line_index`]: which `(evse_index, connector_index)` pair, if any,
/// owns line `line_index` of [`tree_lines`]'s output. Walks the same structure `tree_lines`
/// builds (an EVSE summary line, then either a "no connectors" filler or one line per connector,
/// plus one extra inline-detail line right after the *focused* connector's row when
/// `inline_detail` is set) so the two stay in lockstep - a line that isn't a connector row (an
/// EVSE summary, a filler, or an inline-detail line) yields `None`, as does a `line_index` past
/// the end of the tree.
pub(crate) fn tree_line_to_connector(
    charger: &ChargerState,
    focused: FocusedConnector,
    inline_detail: bool,
    line_index: usize,
) -> Option<(usize, usize)> {
    let mut index = 0;
    for (evse_index, evse) in charger.evses.iter().enumerate() {
        if index == line_index {
            return None; // the EVSE summary line
        }
        index += 1;

        if evse.connectors.is_empty() {
            if index == line_index {
                return None; // "no connectors"
            }
            index += 1;
            continue;
        }

        for connector_index in 0..evse.connectors.len() {
            if index == line_index {
                return Some((evse_index, connector_index));
            }
            index += 1;

            let is_focused = focused.evse == evse_index && focused.connector == connector_index;
            if inline_detail && is_focused {
                if index == line_index {
                    return None; // the inline-detail line
                }
                index += 1;
            }
        }
    }
    None
}

/// Which row of a rendered pane's *content* (i.e. below/inside its chrome) `mouse_row` falls on,
/// or `None` when the click landed on the pane's border/title or outside it entirely. `area` is
/// the pane's outer [`Rect`] as returned by [`dashboard_layout`]/[`body_layout`] - the same one
/// handed to `theme::section`/`theme::header` - not the inner rect a block's chrome already
/// carved out, so this is the single place that has to know a `section` spends its first row on
/// a title rule (see the `HEADER_HEIGHT`/`section` comments above [`dashboard_layout`]).
///
/// Pure `Rect` + `(u16, u16)` math on purpose, so hit-testing is unit-testable without standing
/// up a terminal - see the roadmap's Phase 7 "mouse support" note.
pub(crate) fn content_row_at(area: Rect, mouse_col: u16, mouse_row: u16) -> Option<usize> {
    let content = Rect {
        y: area.y + 1,
        height: area.height.saturating_sub(1),
        ..area
    };
    if !content.contains(ratatui::layout::Position::new(mouse_col, mouse_row)) {
        return None;
    }
    Some((mouse_row - content.y) as usize)
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

/// The tree's current scroll offset for `charger`/`focused` rendered in a `height`-row window -
/// the same value [`render`] computes internally via [`tree_lines`]/[`tree_scroll_offset`],
/// exposed so mouse hit-testing can reconstruct which line of [`tree_lines`]'s output a click's
/// row corresponds to without duplicating the line-counting logic here.
pub(crate) fn tree_scroll_offset_for(
    charger: &ChargerState,
    focused: FocusedConnector,
    inline_detail: bool,
    height: usize,
) -> usize {
    let total = tree_lines(charger, focused, inline_detail).len();
    tree_scroll_offset(total, height, tree_focus_line_index(charger, focused))
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

/// The `Connection` strip's two content lines: why the last CSMS connection attempt failed, and what
/// can be done about it.
///
/// The reason comes straight from `connect_charger`'s error, unabridged apart from fitting the width -
/// a CSMS URL typo, a refused TLS handshake and a rejected password are three different problems and
/// the words are what tell them apart. The second line is the part a bare error message leaves out:
/// after a failed dial nothing else will happen on this screen, so if it doesn't say `r` and `Esc`,
/// the user is looking at a dead end.
fn connection_failure_lines(error: &str, width: usize) -> Vec<Line<'static>> {
    vec![
        Line::from(vec![
            Span::styled("✕ ", theme::error()),
            Span::styled(
                truncate_to_width(error, width.saturating_sub(2)),
                theme::error(),
            ),
        ]),
        Line::from(vec![
            Span::styled("r", theme::accent()),
            Span::styled(": edit the connection and retry    ", theme::text_dim()),
            Span::styled("Esc", theme::accent()),
            Span::styled(": back to the charger list", theme::text_dim()),
        ]),
    ]
}

/// Cuts `text` to `width` columns, marking the cut with `…` so a truncated CSMS error can't be read
/// as the whole of it. Same rule as the picker's column truncation, which has its own copy for the
/// same reason: a silent clip is worse than a visible one.
fn truncate_to_width(text: &str, width: usize) -> String {
    if text.chars().count() <= width {
        return text.to_string();
    }
    if width == 0 {
        return String::new();
    }
    let mut truncated: String = text.chars().take(width - 1).collect();
    truncated.push('…');
    truncated
}

/// Short labels for every capability `config` declares, in a deliberate order: the ones whose
/// behavior a user can watch happen on this screen first, then the ones that only change what the
/// charger tells a CSMS.
///
/// This is the answer to "why did that just refuse?" and "what is my CSMS being told?" - the two
/// questions a declaration actually decides. It lists what the *charger declares*, not what the
/// simulator can do: a flag here is `true` because this charger's YAML (or preset) says so, and
/// `charge_point_simulator_core`'s `SIMULATED_CAPABILITIES` is where "declared" and "simulated"
/// are kept honest with each other.
///
/// `has_display` is deliberately absent, and is the one exception: a charger that declares a display
/// gets a whole `Display` section of its own two rows above this one (see [`body_layout`]), so
/// listing it here would spend a row restating what the screen already shows - the same reason the
/// campaign strip says nothing about an idle installer. Every other declaration has no other
/// representation anywhere on this screen.
fn capability_labels(config: &ChargerConfig) -> Vec<&'static str> {
    let capabilities = &config.capabilities;
    [
        (capabilities.smart_charging, "smart charging"),
        (capabilities.supports_bidirectional_power, "V2G"),
        (capabilities.der_control, "DER control"),
        (capabilities.reservation, "reservation"),
        (capabilities.local_auth_list, "local auth list"),
        (capabilities.firmware_management, "firmware"),
        (capabilities.firmware_publishing, "firmware publishing"),
        (capabilities.diagnostics, "diagnostics"),
        (capabilities.certificate_management, "certificates"),
        (capabilities.has_persistent_storage, "storage"),
        (capabilities.key_storage, "key storage"),
        (capabilities.ocsp_checking, "OCSP"),
        (capabilities.can_unlock_under_load, "unlock under load"),
        (capabilities.has_rtc, "RTC"),
        (capabilities.variable_monitoring, "monitoring"),
        (capabilities.tariff_and_cost, "tariffs"),
        (capabilities.payment, "payment"),
        (capabilities.battery_swap, "battery swap"),
        (capabilities.periodic_event_stream, "event streams"),
    ]
    .into_iter()
    .filter_map(|(declared, label)| declared.then_some(label))
    .collect()
}

/// The capability strip's single line: [`capability_labels`] joined, and - when they don't all fit -
/// truncated with a count of what was dropped rather than cut mid-label.
///
/// `+3 more` is deliberately not silent: a strip that quietly stopped listing would read as "this
/// charger declares nine things" when it declares twelve, and the whole point of the strip is that
/// what a charger claims is exactly what's on it.
fn capability_line(labels: &[&'static str], width: usize) -> Line<'static> {
    const SEPARATOR: &str = "  ·  ";

    let mut shown = labels.len();
    let joined = |count: usize| -> String {
        let mut text = labels[..count].join(SEPARATOR);
        if count < labels.len() {
            text.push_str(&format!("{SEPARATOR}+{} more", labels.len() - count));
        }
        text
    };
    while shown > 1 && joined(shown).chars().count() > width {
        shown -= 1;
    }

    Line::styled(joined(shown), theme::text_dim())
}

/// A text sparkline over `samples`, one cell per sample, scaled to the largest magnitude present -
/// the presentation half of `core`'s [`PowerHistory`], which owns the data (`docs/tui-roadmap.md`'s
/// settled decision 2).
///
/// Empty when there is nothing to plot, so a charger with no history yet draws no line rather than a
/// flat one it would be easy to read as "measured zero for a minute".
///
/// **Magnitude only, and deliberately.** A discharging EVSE's samples are negative (H14), and eight
/// block glyphs cannot show a signed series against a baseline without inventing a second row. The
/// sign is not lost, though: the sidebar states the direction on its own row and prints the signed
/// `kW` figure right below this line - so what the spark adds is the shape of the last minute, which
/// is the thing no single reading can show.
fn power_sparkline(samples: &[f64]) -> String {
    const CELLS: [char; 8] = ['▁', '▂', '▃', '▄', '▅', '▆', '▇', '█'];

    let peak = samples
        .iter()
        .fold(0.0f64, |peak, sample| peak.max(sample.abs()));
    if samples.is_empty() {
        return String::new();
    }
    if peak == 0.0 {
        // Idle for the whole window is a real reading, and the flattest glyph is the honest one for
        // it - unlike an *absent* history, which draws nothing at all.
        return CELLS[0].to_string().repeat(samples.len());
    }

    samples
        .iter()
        .map(|sample| {
            let fraction = (sample.abs() / peak).clamp(0.0, 1.0);
            // `ceil` so any non-zero reading gets at least the lowest visible cell: a connector
            // drawing a trickle must not render identically to one drawing nothing.
            let cell = ((fraction * CELLS.len() as f64).ceil() as usize).clamp(1, CELLS.len());
            CELLS[cell - 1]
        })
        .collect()
}

/// A 10-cell progress bar for an in-flight transfer, the same vocabulary [`soc_bar`] uses so a
/// filled bar means the same thing everywhere on this screen.
fn transfer_bar(fraction: f64) -> String {
    const CELLS: usize = 10;
    let filled = (fraction.clamp(0.0, 1.0) * CELLS as f64).round() as usize;
    format!("{}{}", "█".repeat(filled), "░".repeat(CELLS - filled))
}

/// `3.4/8.0 MiB` - the byte counts a transfer reports, in the unit a firmware image is actually
/// discussed in. Deliberately the same figures the CSMS is being told (see
/// `InFlightTransfer::transferred_bytes`), not a re-derivation from elapsed time.
fn transfer_bytes_text(transfer: &InFlightTransfer) -> String {
    const MIB: f64 = (1024 * 1024) as f64;
    format!(
        "{:.1}/{:.1} MiB",
        transfer.transferred_bytes as f64 / MIB,
        transfer.total_bytes as f64 / MIB
    )
}

/// The word for a firmware install's stage, and the style it earns: a failure is a real failure
/// (the CSMS was told `InstallationFailed`), a completed install is worth a moment of green, and
/// `Installing` is just informational.
fn firmware_stage_text(stage: FirmwareInstallStage) -> (&'static str, Style) {
    match stage {
        FirmwareInstallStage::Idle => ("idle", theme::text_muted()),
        FirmwareInstallStage::Installing => ("installing", theme::text()),
        FirmwareInstallStage::Installed => ("installed", theme::ok()),
        FirmwareInstallStage::Failed => ("failed", theme::error()),
    }
}

/// The campaign strip's single line: whatever of a firmware install, a firmware download and a
/// diagnostics log upload is currently happening (`docs/hardware-roadmap.md`'s H10).
///
/// Only ever rendered when [`crate::app::CampaignProgress::is_active`] says something is - an idle
/// installer produces no line and gets no rows (see [`body_layout`]). Each piece appears
/// independently, because a firmware download and a log upload are independent campaigns that can
/// both be in flight at once.
fn campaign_line(campaigns: &CampaignProgress) -> Line<'static> {
    let mut spans: Vec<Span<'static>> = Vec::new();

    if let Some(download) = &campaigns.firmware_download {
        spans.push(Span::styled("firmware download ", theme::text_dim()));
        spans.push(Span::styled(
            transfer_bar(download.fraction()),
            theme::accent(),
        ));
        spans.push(Span::styled(
            format!(" {}", transfer_bytes_text(download)),
            theme::text(),
        ));
    }

    if let Some(stage) = campaigns.firmware_install.filter(|stage| {
        // An installer that has never been asked to do anything says nothing here; the strip is for
        // activity, and "this charger has an installer" is a capability, not an event.
        !matches!(stage, FirmwareInstallStage::Idle)
    }) {
        if !spans.is_empty() {
            spans.push(Span::raw("   "));
        }
        let (text, style) = firmware_stage_text(stage);
        spans.push(Span::styled("firmware ", theme::text_dim()));
        spans.push(Span::styled(text, style));
    }

    if let Some(upload) = &campaigns.log_upload {
        if !spans.is_empty() {
            spans.push(Span::raw("   "));
        }
        spans.push(Span::styled("log upload ", theme::text_dim()));
        spans.push(Span::styled(
            transfer_bar(upload.fraction()),
            theme::accent(),
        ));
        spans.push(Span::styled(
            format!(" {}", transfer_bytes_text(upload)),
            theme::text(),
        ));
    }

    Line::from(spans)
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
    let strips = body_strips_for(view.charger, &view.campaigns, view.connection_failure);
    let has_display = strips.display;
    let layout = dashboard_layout(frame.area());
    let body = body_layout(layout.body, strips);

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

    if let Some(error) = view.connection_failure.filter(|_| strips.connection) {
        frame.render_widget(
            Paragraph::new(connection_failure_lines(
                error,
                body.connection.width as usize,
            ))
            .block(theme::section("Connection", focused)),
            body.connection,
        );
    }

    if strips.capabilities {
        let capabilities = view
            .charger
            .map(|state| capability_labels(&state.config))
            .unwrap_or_default();
        frame.render_widget(
            Paragraph::new(capability_line(
                &capabilities,
                body.capabilities.width as usize,
            ))
            .block(theme::section("Declared", focused)),
            body.capabilities,
        );
    }

    if strips.campaigns {
        frame.render_widget(
            Paragraph::new(campaign_line(&view.campaigns))
                .block(theme::section("Firmware & files", focused)),
            body.campaigns,
        );
    }

    // A pending attempt outranks a previous failure: if one is in flight, the spinner is the true
    // answer to "what is this link doing", and the strip below still carries the failure it replaced.
    let link = match (view.connecting, view.connection_failure) {
        (true, _) => LinkState::Connecting,
        (false, Some(_)) => LinkState::Failed,
        (false, None) => LinkState::Reported,
    };
    let header_line = match view.charger {
        Some(state) => {
            let segments = header_segments(state, link, layout.header.width as usize);
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
        let body = body_layout(area(120, 30), BodyStrips::default());

        let sidebar = body.sidebar.expect("120 columns should fit a sidebar");
        assert_eq!(body.tree.x, 0);
        assert_eq!(sidebar.x, body.tree.right());
        assert_eq!(sidebar.width, SIDEBAR_WIDTH);
        assert_eq!(body.tree.width + sidebar.width, 120);
    }

    #[test]
    fn body_collapses_the_sidebar_below_the_width_threshold() {
        let body = body_layout(area(80, 30), BodyStrips::default());

        assert!(body.sidebar.is_none());
        assert_eq!(body.tree.width, 80);
    }

    #[test]
    fn the_threshold_itself_still_fits_a_sidebar() {
        let body = body_layout(area(SIDEBAR_MIN_BODY_WIDTH, 30), BodyStrips::default());
        assert!(body.sidebar.is_some());
    }

    #[test]
    fn one_column_below_the_threshold_collapses() {
        let body = body_layout(area(SIDEBAR_MIN_BODY_WIDTH - 1, 30), BodyStrips::default());
        assert!(body.sidebar.is_none());
    }

    #[test]
    fn the_display_strip_takes_no_space_when_the_charger_has_no_display() {
        let body = body_layout(area(120, 30), BodyStrips::default());

        assert_eq!(body.display.height, 0);
        assert_eq!(body.tree.y, body.display.y);
    }

    #[test]
    fn the_display_strip_reserves_its_configured_height_when_present() {
        let body = body_layout(
            area(120, 30),
            BodyStrips {
                display: true,
                ..Default::default()
            },
        );

        assert_eq!(body.display.height, DISPLAY_HEIGHT);
        assert_eq!(body.tree.y, body.display.bottom());
    }

    #[test]
    fn body_layout_does_not_panic_on_a_tiny_area() {
        let body = body_layout(
            area(80, 0),
            BodyStrips {
                display: true,
                ..Default::default()
            },
        );
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

    use charge_point_simulator_core::charger::{CapabilitiesConfig, OcppVersion, Vehicle};

    fn charger_state_for_header(id: &str) -> ChargerState {
        ChargerState::from_config(ChargerConfig {
            id: id.to_string(),
            ocpp_version: OcppVersion::V16J,
            evses: vec![],
            has_display: false,
            capabilities: Default::default(),
        })
    }

    fn segments_text(segments: &[(String, Style)]) -> String {
        segments.iter().map(|(text, _)| text.as_str()).collect()
    }

    #[test]
    fn header_segments_show_local_simulation_for_a_local_charger() {
        let state = charger_state_for_header("CP-CHARGE");
        let text = segments_text(&header_segments(&state, LinkState::Reported, 120));

        assert!(text.contains("CP-CHARGE"));
        assert!(text.contains("local simulation"));
    }

    #[test]
    fn header_segments_show_the_csms_url_for_a_live_csms_charger() {
        let mut state = charger_state_for_header("CP-CHARGE");
        state.mode = SimulationMode::LiveCsms {
            url: "wss://csms.example.com".to_string(),
        };
        let text = segments_text(&header_segments(&state, LinkState::Reported, 120));

        assert!(text.contains("wss://csms.example.com"));
        assert!(!text.contains("local simulation"));
    }

    #[test]
    fn header_segments_show_a_spinner_and_connecting_while_a_connect_attempt_is_pending() {
        let state = charger_state_for_header("CP-CHARGE");
        let text = segments_text(&header_segments(&state, LinkState::Connecting, 120));

        assert!(text.contains("connecting..."));
        // The normal connection-status label (the fresh charger is `Booting`) is replaced,
        // not merely joined by, the spinner - there's only one status slot in the header.
        assert!(!text.contains("booting"));
    }

    #[test]
    fn header_segments_show_the_heartbeat_pulse_alongside_uptime_when_there_is_room() {
        let mut state = charger_state_for_header("CP-CHARGE");
        state.uptime = Duration::from_millis(600);
        let text = segments_text(&header_segments(&state, LinkState::Reported, 120));

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
        let base = header_segments(&state, LinkState::Reported, 0);
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

        let everything = segments_text(&header_segments(
            &state,
            LinkState::Reported,
            with_uptime_width,
        ));
        assert!(everything.contains("local simulation"));
        assert!(everything.contains("up 4m 12s"));

        let mode_only = segments_text(&header_segments(
            &state,
            LinkState::Reported,
            with_uptime_width - 1,
        ));
        assert!(
            mode_only.contains("local simulation"),
            "mode should still fit: {mode_only:?}"
        );
        assert!(
            !mode_only.contains("up 4m 12s"),
            "uptime should have been dropped: {mode_only:?}"
        );

        let floor_only = segments_text(&header_segments(
            &state,
            LinkState::Reported,
            mode_only_width - 1,
        ));
        assert!(
            !floor_only.contains("local simulation"),
            "mode should have been dropped: {floor_only:?}"
        );
        assert!(
            floor_only.contains("CP-CHARGE"),
            "the id/status floor must never be dropped: {floor_only:?}"
        );
    }

    // --- a failed CSMS connection -----------------------------------------------------------

    /// The bug this fixes: after a failed dial the connection thread is gone, so nothing will ever
    /// write `connection_status` again and the charger sits there reporting the `Booting` it was
    /// seeded with. The header must not repeat it.
    #[test]
    fn the_header_says_the_connection_failed_rather_than_repeating_a_stale_booting() {
        let state = charger_state_for_header("CP-2.1");
        assert_eq!(
            state.connection_status,
            ConnectionStatus::Booting,
            "the state a failed dial leaves behind"
        );

        let failed = segments_text(&header_segments(&state, LinkState::Failed, 120));
        assert!(failed.contains("connection failed"), "{failed}");
        assert!(!failed.contains("booting"), "{failed}");

        // And the charger id and mode are still there: which charger, and which CSMS it was.
        assert!(failed.contains("CP-2.1"), "{failed}");
    }

    /// A retry in flight outranks the failure it is retrying: the spinner is the live answer.
    #[test]
    fn a_pending_retry_shows_the_spinner_rather_than_the_previous_failure() {
        let state = charger_state_for_header("CP-2.1");
        let connecting = segments_text(&header_segments(&state, LinkState::Connecting, 120));

        assert!(connecting.contains("connecting..."), "{connecting}");
        assert!(!connecting.contains("connection failed"), "{connecting}");
    }

    #[test]
    fn the_connection_strip_is_only_allotted_rows_when_something_failed() {
        let state = charger_state_for_header("CP-2.1");
        let campaigns = CampaignProgress::default();

        let healthy = body_strips_for(Some(&state), &campaigns, None);
        assert!(!healthy.connection);
        assert_eq!(body_layout(area(120, 30), healthy).connection.height, 0);

        let failed = body_strips_for(Some(&state), &campaigns, Some("connection refused"));
        assert!(failed.connection);
        assert!(body_layout(area(120, 30), failed).connection.height > 0);
    }

    /// The strip carries the reason *and* the way out. An error with no next step is a dead end, and
    /// after a failed dial nothing else on this screen will ever change on its own.
    #[test]
    fn the_connection_strip_names_the_error_and_what_to_do_about_it() {
        let text = lines_text(&connection_failure_lines(
            "handshake failed: certificate has expired",
            120,
        ));

        assert!(
            text.contains("handshake failed: certificate has expired"),
            "{text}"
        );
        assert!(text.contains("retry"), "{text}");
        assert!(text.contains("Esc"), "{text}");
    }

    /// A truncated error must say it was truncated: three different CSMS problems can share a prefix,
    /// and a silently clipped one reads as the whole message.
    #[test]
    fn a_long_error_is_truncated_visibly_and_never_overflows_the_strip() {
        let error = "x".repeat(200);
        let lines = connection_failure_lines(&error, 40);
        let first = lines_text(&lines[..1]);

        assert!(
            first.chars().count() <= 40,
            "{} columns",
            first.chars().count()
        );
        assert!(first.ends_with('…'), "{first}");
    }

    // --- the power sparkline (settled decision 2) ------------------------------------------

    #[test]
    fn no_history_draws_no_sparkline_at_all() {
        assert_eq!(power_sparkline(&[]), "");
    }

    /// An EVSE that really was idle for the window is a different thing from one with no history:
    /// the first is a measurement, the second is an absence.
    #[test]
    fn an_idle_window_draws_the_flattest_line_rather_than_nothing() {
        assert_eq!(power_sparkline(&[0.0, 0.0, 0.0]), "▁▁▁");
    }

    #[test]
    fn a_sparkline_scales_to_the_largest_magnitude_it_holds() {
        assert_eq!(power_sparkline(&[0.0, 3.7, 7.4]), "▁▄█");
        // Scaled to its own peak, so the same shape at a tenth the power reads the same - a
        // sparkline is about the trend; the kW figure beside it carries the absolute value.
        assert_eq!(power_sparkline(&[0.0, 0.37, 0.74]), "▁▄█");
    }

    /// A trickle must not render identically to nothing at all, which is what a flooring
    /// implementation would do to it.
    #[test]
    fn any_non_zero_reading_gets_at_least_the_lowest_visible_cell() {
        let spark = power_sparkline(&[0.001, 7.4]);
        assert_eq!(spark.chars().next(), Some('▁'));
        assert_ne!(spark.chars().next(), Some(' '));
    }

    /// H14: exported power is negative, and magnitude is what the spark plots - see
    /// [`power_sparkline`] for why, and where the direction is stated instead.
    #[test]
    fn exporting_and_importing_the_same_power_draw_the_same_shape() {
        assert_eq!(power_sparkline(&[-7.4, -3.7]), power_sparkline(&[7.4, 3.7]));
    }

    #[test]
    fn the_sidebar_shows_the_window_it_is_plotting_and_only_once_there_is_history() {
        let mut evse = evse_with_statuses(&[ConnectorStatus::Charging]);
        let connector = connector_with_hardware(true, true, None, false, 0);

        let without_history = lines_text(&sidebar_lines(&evse, &connector));
        // The SoC bar uses `█`/`░` of its own, so the anchor here is the spark's window label - the
        // one thing only the spark row carries.
        assert!(!without_history.contains("  5s"), "{without_history}");
        assert!(!without_history.contains('▁'), "{without_history}");

        evse.metrics.power_kw = 7.4;
        for _ in 0..5 {
            evse.tick(PowerHistory::SAMPLE_INTERVAL);
        }

        let with_history = lines_text(&sidebar_lines(&evse, &connector));
        assert!(with_history.contains("█████  5s"), "{with_history}");
    }

    /// The sidebar is 32 columns wide, so the spark is capped rather than clipped - a series cut off
    /// with no indication would misreport how long the window is.
    #[test]
    fn the_sidebar_sparkline_never_outgrows_the_panel() {
        let mut evse = evse_with_statuses(&[ConnectorStatus::Charging]);
        evse.metrics.power_kw = 7.4;
        for _ in 0..PowerHistory::CAPACITY {
            evse.tick(PowerHistory::SAMPLE_INTERVAL);
        }
        let connector = connector_with_hardware(true, true, None, false, 0);

        let text = lines_text(&sidebar_lines(&evse, &connector));
        // The last such line, not the first: the SoC bar above uses the same full block.
        let spark_line = text
            .lines()
            .rfind(|line| line.contains('█'))
            .expect("a full window draws a spark");

        assert!(
            spark_line.chars().count() <= SIDEBAR_WIDTH as usize,
            "{} columns in a {SIDEBAR_WIDTH}-column sidebar: {spark_line}",
            spark_line.chars().count()
        );
        assert!(
            spark_line.ends_with(&format!("{SIDEBAR_SPARKLINE_CELLS}s")),
            "the label has to say the window actually shown: {spark_line}"
        );
    }

    // --- the declared-capabilities strip (H4) ----------------------------------------------

    fn config_with(declare: impl FnOnce(&mut CapabilitiesConfig)) -> ChargerConfig {
        let mut config = ChargerConfig {
            id: "CP-CAPS".to_string(),
            ocpp_version: OcppVersion::V21,
            evses: vec![],
            has_display: false,
            capabilities: CapabilitiesConfig::default(),
        };
        declare(&mut config.capabilities);
        config
    }

    #[test]
    fn a_charger_declaring_nothing_gets_no_capability_strip_at_all() {
        let config = config_with(|_| {});
        assert!(capability_labels(&config).is_empty());

        let state = ChargerState::from_config(config);
        let strips = body_strips_for(Some(&state), &CampaignProgress::default(), None);
        assert!(!strips.capabilities);
        assert_eq!(
            body_layout(area(120, 30), strips).capabilities.height,
            0,
            "an empty list must not cost the tree two rows"
        );
    }

    /// A display has its own section; every other declaration has no other representation on this
    /// screen, which is what the strip is for - see [`capability_labels`].
    #[test]
    fn a_declared_display_is_left_to_the_display_section_rather_than_listed_twice() {
        let mut config = config_with(|_| {});
        config.has_display = true;
        assert!(capability_labels(&config).is_empty());

        let both_spellings = config_with(|capabilities| capabilities.has_display = true);
        assert!(capability_labels(&both_spellings).is_empty());
    }

    #[test]
    fn declared_capabilities_are_listed_behaviour_first() {
        let config = config_with(|capabilities| {
            capabilities.smart_charging = true;
            capabilities.supports_bidirectional_power = true;
            capabilities.has_persistent_storage = true;
            capabilities.payment = true;
        });

        assert_eq!(
            capability_labels(&config),
            vec!["smart charging", "V2G", "storage", "payment"]
        );
    }

    #[test]
    fn an_undeclared_capability_is_never_listed() {
        let config = config_with(|capabilities| capabilities.smart_charging = true);
        let labels = capability_labels(&config);

        assert_eq!(labels, vec!["smart charging"]);
        assert!(!labels.contains(&"V2G"));
        assert!(!labels.contains(&"firmware"));
    }

    /// Truncation says how much it dropped: a list that quietly stopped would read as a shorter
    /// declaration than the charger actually made.
    #[test]
    fn a_capability_list_too_wide_to_fit_says_how_many_it_dropped() {
        let labels = vec!["smart charging", "V2G", "DER control", "reservation"];

        let full = lines_text(&[capability_line(&labels, 120)]);
        assert_eq!(
            full,
            "smart charging  ·  V2G  ·  DER control  ·  reservation"
        );
        assert!(!full.contains("more"));

        let narrow = lines_text(&[capability_line(&labels, 40)]);
        assert!(narrow.chars().count() <= 40, "{narrow}");
        assert!(narrow.starts_with("smart charging"), "{narrow}");
        assert!(narrow.ends_with("more"), "{narrow}");

        // Pathologically narrow: one label always survives rather than an empty or mid-word line.
        let tiny = lines_text(&[capability_line(&labels, 1)]);
        assert!(tiny.starts_with("smart charging"), "{tiny}");
        assert!(tiny.ends_with("+3 more"), "{tiny}");
    }

    // --- the campaign strip: firmware and file transfers (H10) -----------------------------

    fn transfer(fraction: f64, total_bytes: u64) -> InFlightTransfer {
        InFlightTransfer {
            elapsed: Duration::from_secs(10).mul_f64(fraction),
            duration: Duration::from_secs(10),
            transferred_bytes: (total_bytes as f64 * fraction) as u64,
            total_bytes,
        }
    }

    #[test]
    fn the_campaign_strip_is_only_allotted_rows_while_something_is_in_flight() {
        let idle = body_layout(area(120, 30), BodyStrips::default());
        assert_eq!(idle.campaigns.height, 0);
        let tree_without_strip = idle.tree.height;

        let active = body_layout(
            area(120, 30),
            BodyStrips {
                campaigns: true,
                ..Default::default()
            },
        );
        assert!(active.campaigns.height > 0);
        assert_eq!(
            active.tree.height,
            tree_without_strip - active.campaigns.height,
            "the strip's rows have to come from the tree, not from nowhere"
        );
    }

    /// A firmware download and a log upload are independent campaigns that can both be in flight at
    /// once, so the strip has to be able to show both - the reason `FakeFileTransfer` fails them
    /// independently in the first place.
    #[test]
    fn the_campaign_strip_shows_a_download_an_install_and_an_upload_together() {
        let text = lines_text(&[campaign_line(&CampaignProgress {
            firmware_install: Some(FirmwareInstallStage::Installing),
            firmware_download: Some(transfer(0.5, 8 * 1024 * 1024)),
            log_upload: Some(transfer(0.25, 2 * 1024 * 1024)),
        })]);

        assert!(text.contains("firmware download"), "{text}");
        assert!(text.contains("4.0/8.0 MiB"), "{text}");
        assert!(text.contains("firmware installing"), "{text}");
        assert!(text.contains("log upload"), "{text}");
        assert!(text.contains("0.5/2.0 MiB"), "{text}");
    }

    /// An installer that exists but has never been asked to do anything is a capability, not an
    /// event - the strip is for activity.
    #[test]
    fn an_idle_installer_puts_nothing_in_the_campaign_strip() {
        let text = lines_text(&[campaign_line(&CampaignProgress {
            firmware_install: Some(FirmwareInstallStage::Idle),
            ..Default::default()
        })]);

        assert_eq!(text, "");
    }

    #[test]
    fn a_failed_install_is_named_as_failed_rather_than_silently_dropped() {
        let text = lines_text(&[campaign_line(&CampaignProgress {
            firmware_install: Some(FirmwareInstallStage::Failed),
            ..Default::default()
        })]);

        assert!(text.contains("firmware failed"), "{text}");
    }

    #[test]
    fn a_transfer_bar_fills_with_its_fraction() {
        assert_eq!(transfer_bar(0.0), "░░░░░░░░░░");
        assert_eq!(transfer_bar(0.5), "█████░░░░░");
        assert_eq!(transfer_bar(1.0), "██████████");
        // Nothing outside 0..=1 can produce a bar of the wrong width.
        assert_eq!(transfer_bar(-1.0).chars().count(), 10);
        assert_eq!(transfer_bar(2.0), "██████████");
    }

    // --- hardware state: lock, contactor, current limit, direction (H7/H14) ----------------

    #[test]
    fn lock_and_contactor_always_read_as_one_of_two_states_never_blank() {
        assert_eq!(lock_text(true), "engaged");
        assert_eq!(lock_text(false), "released");
        assert_eq!(contactor_text(true), "closed");
        assert_eq!(contactor_text(false), "open");
    }

    /// The distinction `ConnectorState::current_limit_ma` exists to preserve has to survive being
    /// rendered: a suspended connector (`Some(0)`) must not read the same as an unlimited one
    /// (`None`).
    #[test]
    fn a_suspended_current_limit_never_renders_the_same_as_no_limit_at_all() {
        assert_eq!(current_limit_text(None), "none");
        assert_eq!(current_limit_text(Some(0)), "suspended (0.0 A)");
        assert_eq!(current_limit_text(Some(16_000)), "16.0 A");
        assert_eq!(current_limit_text(Some(6_500)), "6.5 A");
        assert_ne!(current_limit_text(Some(0)), current_limit_text(None));
    }

    #[test]
    fn exported_energy_reads_in_kwh_like_every_other_energy_figure() {
        assert_eq!(exported_energy_text(0), "0.000 kWh");
        assert_eq!(exported_energy_text(1_250), "1.250 kWh");
    }

    /// A connector with a mid-session vehicle, plus whatever hardware state the caller wants.
    fn connector_with_hardware(
        locked: bool,
        contactor_closed: bool,
        current_limit_ma: Option<u32>,
        discharging: bool,
        exported_energy_wh: i64,
    ) -> ConnectorState {
        ConnectorState {
            id: 1,
            status: ConnectorStatus::Charging,
            vehicle: Some(Vehicle {
                id: "MY-EV-1".into(),
                state_of_charge: Some(34.0),
            }),
            session_duration: Duration::from_secs(72),
            locked,
            contactor_closed,
            current_limit_ma,
            discharging,
            exported_energy_wh,
        }
    }

    fn lines_text(lines: &[Line<'static>]) -> String {
        lines
            .iter()
            .map(|line| {
                line.spans
                    .iter()
                    .map(|span| span.content.as_ref())
                    .collect::<String>()
            })
            .collect::<Vec<_>>()
            .join("\n")
    }

    #[test]
    fn the_sidebar_states_lock_contactor_and_limit_even_when_none_of_them_is_engaged() {
        let evse = evse_with_statuses(&[ConnectorStatus::Charging]);
        let connector = connector_with_hardware(false, false, None, false, 0);

        let text = lines_text(&sidebar_lines(&evse, &connector));

        assert!(text.contains("lock     released"), "{text}");
        assert!(text.contains("contact  open"), "{text}");
        assert!(text.contains("limit    none"), "{text}");
    }

    #[test]
    fn the_sidebar_shows_engaged_hardware_and_the_applied_limit() {
        let evse = evse_with_statuses(&[ConnectorStatus::Charging]);
        let connector = connector_with_hardware(true, true, Some(16_000), false, 0);

        let text = lines_text(&sidebar_lines(&evse, &connector));

        assert!(text.contains("lock     engaged"), "{text}");
        assert!(text.contains("contact  closed"), "{text}");
        assert!(text.contains("limit    16.0 A"), "{text}");
    }

    /// Direction is only worth a row when it isn't the default; the export register outlives the
    /// discharge that filled it, so it is shown whenever it's non-zero.
    #[test]
    fn the_sidebar_calls_out_discharge_and_keeps_the_export_register_afterwards() {
        let evse = evse_with_statuses(&[ConnectorStatus::Charging]);

        let importing = lines_text(&sidebar_lines(
            &evse,
            &connector_with_hardware(true, true, None, false, 0),
        ));
        assert!(!importing.contains("exporting"), "{importing}");
        assert!(!importing.contains("exported"), "{importing}");

        let exporting = lines_text(&sidebar_lines(
            &evse,
            &connector_with_hardware(true, true, None, true, 2_500),
        ));
        assert!(exporting.contains("exporting (V2G)"), "{exporting}");
        assert!(exporting.contains("exported 2.500 kWh"), "{exporting}");

        let after = lines_text(&sidebar_lines(
            &evse,
            &connector_with_hardware(true, true, None, false, 2_500),
        ));
        assert!(!after.contains("exporting (V2G)"), "{after}");
        assert!(after.contains("exported 2.500 kWh"), "{after}");
    }

    /// The narrow fallback carries the two hardware facts that change what a user does next, only
    /// when they aren't the default, and has to stay inside 80 columns while doing it - the width
    /// the `dashboard_narrow` golden renders at (see [`inline_detail_line`]).
    #[test]
    fn the_inline_detail_appends_the_limit_and_direction_only_when_they_are_not_the_default() {
        let evse = evse_with_statuses(&[ConnectorStatus::Charging]);

        let default_hardware = lines_text(&[inline_detail_line(
            &evse,
            &connector_with_hardware(true, true, None, false, 0),
        )]);
        assert!(!default_hardware.contains(" A"), "{default_hardware}");
        assert!(!default_hardware.contains("V2G"), "{default_hardware}");

        let limited_and_exporting = lines_text(&[inline_detail_line(
            &evse,
            &connector_with_hardware(true, true, Some(16_000), true, 0),
        )]);
        assert!(
            limited_and_exporting.contains("16.0 A"),
            "{limited_and_exporting}"
        );
        assert!(
            limited_and_exporting.contains("V2G"),
            "{limited_and_exporting}"
        );
        assert!(
            limited_and_exporting.chars().count() <= 80,
            "the inline detail must survive the narrow layout it exists for: \
             {} columns: {limited_and_exporting}",
            limited_and_exporting.chars().count()
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
                    locked: false,
                    contactor_closed: false,
                    current_limit_ma: None,
                    discharging: false,
                    exported_energy_wh: 0,
                })
                .collect(),
            metrics: Default::default(),
            power_history: Default::default(),
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
    fn tree_line_to_connector_finds_the_connector_at_a_row() {
        let charger = charger_with_evses(vec![
            evse_with_statuses(&[ConnectorStatus::Available, ConnectorStatus::Available]),
            evse_with_statuses(&[ConnectorStatus::Available]),
        ]);
        let focused = FocusedConnector {
            evse: 0,
            connector: 0,
        };

        // EVSE 1 summary (0), connector 1 (1), connector 2 (2), EVSE 2 summary (3), connector 1 (4)
        assert_eq!(
            tree_line_to_connector(&charger, focused, false, 2),
            Some((0, 1))
        );
        assert_eq!(
            tree_line_to_connector(&charger, focused, false, 4),
            Some((1, 0))
        );
    }

    #[test]
    fn tree_line_to_connector_is_none_for_evse_summary_and_filler_lines() {
        let charger = charger_with_evses(vec![
            evse_with_statuses(&[ConnectorStatus::Available]),
            evse_with_statuses(&[]),
        ]);
        let focused = FocusedConnector::default();

        assert_eq!(tree_line_to_connector(&charger, focused, false, 0), None); // EVSE 1 summary
        assert_eq!(tree_line_to_connector(&charger, focused, false, 2), None); // EVSE 2 summary
        assert_eq!(tree_line_to_connector(&charger, focused, false, 3), None); // "no connectors"
    }

    #[test]
    fn tree_line_to_connector_skips_the_inline_detail_line_after_the_focused_row() {
        let charger = charger_with_evses(vec![evse_with_statuses(&[
            ConnectorStatus::Available,
            ConnectorStatus::Available,
        ])]);
        let focused = FocusedConnector {
            evse: 0,
            connector: 0,
        };

        // EVSE summary (0), C1 focused (1), inline detail (2), C2 (3)
        assert_eq!(
            tree_line_to_connector(&charger, focused, true, 1),
            Some((0, 0))
        );
        assert_eq!(tree_line_to_connector(&charger, focused, true, 2), None);
        assert_eq!(
            tree_line_to_connector(&charger, focused, true, 3),
            Some((0, 1))
        );
    }

    #[test]
    fn tree_line_to_connector_is_none_past_the_end() {
        let charger = charger_with_evses(vec![evse_with_statuses(&[ConnectorStatus::Available])]);
        let focused = FocusedConnector::default();

        assert_eq!(tree_line_to_connector(&charger, focused, false, 99), None);
    }

    // --- content_row_at ------------------------------------------------------------------

    #[test]
    fn content_row_at_skips_the_top_chrome_row() {
        let pane = area(20, 10);
        assert_eq!(content_row_at(pane, 5, 0), None); // the title/rule row
        assert_eq!(content_row_at(pane, 5, 1), Some(0));
        assert_eq!(content_row_at(pane, 5, 9), Some(8));
    }

    #[test]
    fn content_row_at_is_none_outside_the_pane() {
        let pane = Rect::new(10, 5, 20, 10);
        assert_eq!(content_row_at(pane, 5, 6), None); // left of the pane
        assert_eq!(content_row_at(pane, 10, 20), None); // below the pane
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

    #[test]
    fn tree_scroll_offset_for_matches_a_manual_computation() {
        let charger = charger_with_evses(vec![evse_with_statuses(&[
            ConnectorStatus::Available,
            ConnectorStatus::Available,
            ConnectorStatus::Available,
            ConnectorStatus::Available,
        ])]);
        let focused = FocusedConnector {
            evse: 0,
            connector: 3,
        };

        // 5 lines total (1 summary + 4 connectors), a 3-row window, focus on the last line (4):
        // offset must be 4 + 1 - 3 = 2.
        assert_eq!(tree_scroll_offset_for(&charger, focused, false, 3), 2);
    }
}
