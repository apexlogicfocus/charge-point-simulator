//! Golden-file snapshot tests for the TUI's rendering.
//!
//! Each test builds an [`App`] in a specific, fully-deterministic state (never touching real
//! time, randomness, or the filesystem) and renders it with ratatui's [`TestBackend`] to a
//! plain-text grid, then compares that text against a checked-in golden file under
//! `crates/charge_point_simulator_tui/snapshots/`.
//!
//! These goldens exist to act as a regression net for an upcoming rendering refactor: as long
//! as every scenario here still matches its golden, the refactor hasn't changed what's on
//! screen. They intentionally capture *text only*, not styles/colors - a later phase changes
//! the color scheme extensively, and style-sensitive goldens would just be constant churn
//! without protecting against the kind of regression (misplaced/garbled text, wrong layout)
//! this harness is meant to catch.
//!
//! To (re)generate every golden from the current rendering:
//! ```sh
//! UPDATE_SNAPSHOTS=1 cargo test -p charge_point_simulator_tui
//! ```

use std::path::PathBuf;
use std::time::Duration;

use crate::app::App;
use crate::screen::Screen;
use crate::text_field::TextField;
use charge_point_simulator_core::charger::{
    ChargerConfig, ChargerEntry, ChargerSource, ChargerState, Command, ConnectionStatus,
    EvseConfig, OcppVersion,
};
use ratatui::Terminal;
use ratatui::backend::TestBackend;

/// Renders `app` at `width`x`height` and flattens the result to plain text: one line per
/// terminal row, each right-trimmed of trailing spaces, rows joined with `\n`. Only the
/// character content of each cell is captured - see the module doc comment for why styles
/// are deliberately left out.
fn render(app: &App, width: u16, height: u16) -> String {
    let backend = TestBackend::new(width, height);
    let mut terminal = Terminal::new(backend).expect("TestBackend::new never fails to initialize");
    terminal
        .draw(|frame| app.draw(frame))
        .expect("drawing to an in-memory TestBackend never fails");

    let buffer = terminal.backend().buffer();
    (0..height)
        .map(|y| {
            let mut line = String::with_capacity(width as usize);
            for x in 0..width {
                line.push_str(buffer[(x, y)].symbol());
            }
            line.trim_end().to_string()
        })
        .collect::<Vec<_>>()
        .join("\n")
}

fn snapshot_path(name: &str) -> PathBuf {
    PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("snapshots").join(format!("{name}.txt"))
}

/// Compares `actual` against the golden file `snapshots/{name}.txt`, either updating it (when
/// `UPDATE_SNAPSHOTS=1` is set) or panicking with a readable line-by-line diff.
fn assert_snapshot(name: &str, actual: &str) {
    let path = snapshot_path(name);

    if std::env::var("UPDATE_SNAPSHOTS").as_deref() == Ok("1") {
        if let Some(parent) = path.parent() {
            std::fs::create_dir_all(parent).expect("failed to create the snapshots directory");
        }
        std::fs::write(&path, actual).expect("failed to write the snapshot golden file");
        return;
    }

    let expected = std::fs::read_to_string(&path).unwrap_or_else(|_| {
        panic!(
            "missing snapshot golden file {path}\n\
             re-run with `UPDATE_SNAPSHOTS=1 cargo test -p charge_point_simulator_tui` to create it",
            path = path.display()
        )
    });

    if expected != actual {
        panic!("{}", diff_report(name, &expected, actual));
    }
}

/// Builds a readable report of every differing row between `expected` and `actual`, so a
/// failure can be understood from the test output alone without opening either file.
fn diff_report(name: &str, expected: &str, actual: &str) -> String {
    let expected_lines: Vec<&str> = expected.lines().collect();
    let actual_lines: Vec<&str> = actual.lines().collect();
    let row_count = expected_lines.len().max(actual_lines.len());

    let mut report = format!("snapshot mismatch: {name}\n");
    for row in 0..row_count {
        let expected_line = expected_lines.get(row).copied();
        let actual_line = actual_lines.get(row).copied();
        if expected_line != actual_line {
            report.push_str(&format!(
                "  row {row}:\n    expected: {:?}\n    actual:   {:?}\n",
                expected_line.unwrap_or("<no such row>"),
                actual_line.unwrap_or("<no such row>"),
            ));
        }
    }
    if expected_lines.len() != actual_lines.len() {
        report.push_str(&format!(
            "  row count differs: expected {} rows, actual {} rows\n",
            expected_lines.len(),
            actual_lines.len()
        ));
    }
    report.push_str(
        "  if this change is intended, re-run with \
         `UPDATE_SNAPSHOTS=1 cargo test -p charge_point_simulator_tui` to accept it\n",
    );
    report
}

fn charger_config(id: &str, ocpp_version: OcppVersion, evses: Vec<EvseConfig>, has_display: bool) -> ChargerConfig {
    ChargerConfig { id: id.to_string(), ocpp_version, evses, has_display }
}

fn charger_entry(id: &str, ocpp_version: OcppVersion, evses: Vec<EvseConfig>) -> ChargerEntry {
    ChargerEntry {
        config: charger_config(id, ocpp_version, evses, false),
        source: ChargerSource::BuiltIn,
    }
}

/// An `App` already on the dashboard for `config`, connected (rather than the freshly-seeded
/// `Booting` status `ChargerState::from_config` starts in - a "just booted" dashboard isn't
/// what most of these scenarios are trying to capture).
fn dashboard_app(config: ChargerConfig) -> App {
    let mut app = App::new(vec![]);
    app.screen = Screen::Dashboard;
    let mut state = ChargerState::from_config(config);
    state.connection_status = ConnectionStatus::Connected;
    app.charger_state = Some(state);
    app
}

/// A dashboard with a single EVSE whose one connector has a vehicle plugged in and charging
/// (RFID presented), simulated forward by a fixed 600s so the meter readings are non-zero, plus
/// a dozen log lines so the log pane is scrolled full. Shared by every scenario that wants a
/// "mid-session" dashboard as its base (the plain charging view, and both command-palette
/// variants opened on top of it).
fn charging_dashboard_app() -> App {
    let config = charger_config("CP-CHARGE", OcppVersion::V16J, vec![EvseConfig { id: 1, connectors: 1 }], false);
    let mut app = dashboard_app(config);
    let state = app.charger_state.as_mut().unwrap();
    Command::PlugInVehicle.apply(&mut state.evses[0], "MY-EV-1");
    Command::PresentRfid.apply(&mut state.evses[0], "TAG-42");
    // A fixed, non-wall-clock elapsed time - this mirrors what `App::tick_metrics_with` does to
    // `charger_state` (`state.tick(elapsed)`); `tick_metrics_with` itself is private to the
    // `app` module and out of reach from here, but the rest of what it does
    // (`maybe_send_meter_values`) is a no-op anyway without a live `ocpp_event_sender`, which
    // none of these scenarios set up.
    state.tick(Duration::from_secs(600));

    for i in 1..=12 {
        app.logs.push(format!("event {i}: heartbeat sent"));
    }

    app
}

#[test]
fn picker() {
    let app = App::new(vec![
        charger_entry("CP001", OcppVersion::V16J, vec![EvseConfig { id: 1, connectors: 1 }]),
        charger_entry(
            "CP002",
            OcppVersion::V201,
            vec![
                EvseConfig { id: 1, connectors: 2 },
                EvseConfig { id: 2, connectors: 1 },
            ],
        ),
        charger_entry("CP-2.1", OcppVersion::V21, vec![EvseConfig { id: 1, connectors: 1 }]),
    ]);

    assert_snapshot("picker", &render(&app, 120, 34));
}

#[test]
fn picker_filtered() {
    let mut app = App::new(vec![
        charger_entry("CP001", OcppVersion::V16J, vec![EvseConfig { id: 1, connectors: 1 }]),
        charger_entry("CP002", OcppVersion::V201, vec![EvseConfig { id: 1, connectors: 1 }]),
        charger_entry("CP-2.1", OcppVersion::V21, vec![EvseConfig { id: 1, connectors: 1 }]),
    ]);
    app.picker_filter = TextField::new("cp0");

    assert_snapshot("picker_filtered", &render(&app, 120, 34));
}

#[test]
fn picker_no_matches() {
    let mut app = App::new(vec![charger_entry(
        "CP001",
        OcppVersion::V16J,
        vec![EvseConfig { id: 1, connectors: 1 }],
    )]);
    app.picker_filter = TextField::new("zzz");

    assert_snapshot("picker_no_matches", &render(&app, 120, 34));
}

#[test]
fn dashboard_idle() {
    let config = charger_config(
        "CP-IDLE",
        OcppVersion::V16J,
        vec![
            EvseConfig { id: 1, connectors: 1 },
            EvseConfig { id: 2, connectors: 2 },
        ],
        false,
    );
    let app = dashboard_app(config);

    assert_snapshot("dashboard_idle", &render(&app, 120, 34));
}

#[test]
fn dashboard_charging() {
    let app = charging_dashboard_app();

    assert_snapshot("dashboard_charging", &render(&app, 120, 34));
}

#[test]
fn dashboard_with_display() {
    let config = charger_config("CP-DISPLAY", OcppVersion::V16J, vec![EvseConfig { id: 1, connectors: 1 }], true);
    let mut app = dashboard_app(config);
    let state = app.charger_state.as_mut().unwrap();
    Command::SetDisplayMessage.apply_to_charger(state, "Welcome to Flowion");

    assert_snapshot("dashboard_with_display", &render(&app, 120, 34));
}

#[test]
fn dashboard_faulted() {
    let config = charger_config("CP-FAULT", OcppVersion::V16J, vec![EvseConfig { id: 1, connectors: 1 }], false);
    let mut app = dashboard_app(config);
    let state = app.charger_state.as_mut().unwrap();
    Command::ReportFault.apply(&mut state.evses[0], "OverCurrentFailure");

    assert_snapshot("dashboard_faulted", &render(&app, 120, 34));
}

#[test]
fn command_palette() {
    let mut app = charging_dashboard_app();
    app.command_palette_open = true;
    app.command_palette_selected = 0;

    assert_snapshot("command_palette", &render(&app, 120, 34));
}

#[test]
fn command_palette_filtered() {
    let mut app = charging_dashboard_app();
    app.command_palette_open = true;
    app.command_palette_filter = TextField::new("unplug");

    assert_snapshot("command_palette_filtered", &render(&app, 120, 34));
}

#[test]
fn parameter_prompt() {
    let config = charger_config("CP001", OcppVersion::V16J, vec![EvseConfig { id: 1, connectors: 1 }], false);
    let mut app = dashboard_app(config);
    app.parameter_prompt = Some(Command::PlugInVehicle);
    app.parameter_field = TextField::new("MY-EV-1");

    assert_snapshot("parameter_prompt", &render(&app, 120, 34));
}

#[test]
fn help_overlay() {
    let config = charger_config("CP001", OcppVersion::V16J, vec![EvseConfig { id: 1, connectors: 1 }], false);
    let mut app = dashboard_app(config);
    app.help_open = true;

    assert_snapshot("help_overlay", &render(&app, 120, 34));
}

#[test]
fn quit_confirm() {
    let config = charger_config("CP001", OcppVersion::V16J, vec![EvseConfig { id: 1, connectors: 1 }], false);
    let mut app = dashboard_app(config);
    app.quit_confirm_open = true;

    assert_snapshot("quit_confirm", &render(&app, 120, 34));
}

#[test]
fn connection_setup() {
    let mut app = App::new(vec![]);
    app.screen = Screen::ConnectionSetup;
    app.connection_csms_url = TextField::new("wss://csms.example.com");
    app.connection_ocpp_identity = TextField::new("CP-2.1");
    app.connection_password = TextField::new("secret");
    app.connection_focused_field = 0;

    assert_snapshot("connection_setup", &render(&app, 120, 34));
}

#[test]
fn dashboard_narrow() {
    let app = charging_dashboard_app();

    assert_snapshot("dashboard_narrow", &render(&app, 80, 24));
}

#[test]
fn terminal_too_small() {
    let app = App::new(vec![]);

    assert_snapshot("terminal_too_small", &render(&app, 50, 10));
}
