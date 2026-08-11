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

use crate::app::{App, CampaignProgress, FocusedConnector};
use crate::logs::{Direction, LogEntry, LogLevel};
use crate::screen::Screen;
use crate::text_field::TextField;
use charge_point_simulator_core::charger::{
    ChargerConfig, ChargerEntry, ChargerSource, ChargerState, Command, ConnectionProfile,
    ConnectionStatus, EvseConfig, EvseMetrics, FirmwareInstallStage, InFlightTransfer, OcppVersion,
    SecurityProfile, SimulationMode, built_in_chargers,
};
use crossterm::event::{KeyCode, KeyEvent, KeyModifiers};
use ratatui::Terminal;
use ratatui::backend::TestBackend;

/// Renders `app` at `width`x`height` and flattens the result to plain text: one line per
/// terminal row, each right-trimmed of trailing spaces, rows joined with `\n`. Only the
/// character content of each cell is captured - see the module doc comment for why styles
/// are deliberately left out.
fn render(app: &mut App, width: u16, height: u16) -> String {
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
    PathBuf::from(env!("CARGO_MANIFEST_DIR"))
        .join("snapshots")
        .join(format!("{name}.txt"))
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

fn charger_config(
    id: &str,
    ocpp_version: OcppVersion,
    evses: Vec<EvseConfig>,
    has_display: bool,
) -> ChargerConfig {
    ChargerConfig {
        id: id.to_string(),
        ocpp_version,
        evses,
        has_display,
        capabilities: Default::default(),
    }
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
    let config = charger_config(
        "CP-CHARGE",
        OcppVersion::V16J,
        vec![EvseConfig {
            id: 1,
            connectors: 1,
        }],
        false,
    );
    let mut app = dashboard_app(config);
    let state = app.charger_state.as_mut().unwrap();
    Command::PlugInVehicle.apply_to(&mut state.evses[0], 0, "MY-EV-1");
    Command::PresentRfid.apply_to(&mut state.evses[0], 0, "TAG-42");
    // H3b moved the meter itself into `core`'s hardware layer, reached only through a real
    // `RunningCharger` and `apply_ocpp_state` - out of reach from a bare `ChargerState` fixture
    // like this one, which renders straight from `App::charger_state` with no running charger
    // behind it. Setting the reading `EvseState::tick`'s old accumulator would have produced for
    // 600 simulated seconds at 7.4 kW keeps this "mid-session" fixture's numbers meaningful
    // without standing up a whole runtime just to render a snapshot.
    state.evses[0].metrics = EvseMetrics {
        power_kw: 7.4,
        current_a: 7.4 * 1000.0 / 230.0,
        energy_kwh: 7.4 * 600.0 / 3600.0,
    };
    // A fixed, non-wall-clock elapsed time - this mirrors what `App::tick_metrics_with` does to
    // `charger_state` (`state.tick(elapsed)`), for the session-duration/SoC bookkeeping that
    // still lives there, and for the power history the sidebar's sparkline plots. Ticked *after*
    // the reading is set, in the order the real app runs them (snapshot drained, then ticked), so
    // the history records this session's 7.4 kW rather than a window of zeros.
    state.tick(Duration::from_secs(600));
    // The hardware-only half of the same reading (H7), set for exactly the same reason and with the
    // same honesty constraint: a connector genuinely mid-session has its cable locked and its
    // contactor closed - that is *why* the meter above is moving - here under a CSMS-applied 16 A
    // charging profile. Left at their construction defaults, the goldens would show a charging
    // connector as unlocked with its contactor open, a state real hardware never reaches.
    state.evses[0].connectors[0].locked = true;
    state.evses[0].connectors[0].contactor_closed = true;
    state.evses[0].connectors[0].current_limit_ma = Some(16_000);

    // Structured entries rather than plain strings, so the goldens actually pin the log pane's
    // columns: timestamp, level, direction marker, elided target, action, and fields.
    // Timestamps are hand-written (never a wall clock) so the goldens stay deterministic.
    for i in 1..=12 {
        let outbound = i % 2 == 1;
        app.logs.push(LogEntry {
            timestamp: Some(format!("10:30:{i:02}.000")),
            level: if i == 4 {
                LogLevel::Warn
            } else {
                LogLevel::Info
            },
            target: "charge_point_simulator_core::charger::state".to_string(),
            message: if outbound {
                "heartbeat sent".to_string()
            } else {
                "heartbeat acknowledged".to_string()
            },
            fields: vec![("seq".to_string(), i.to_string())],
            direction: Some(if outbound {
                Direction::Outbound
            } else {
                Direction::Inbound
            }),
            action: Some("Heartbeat".to_string()),
        });
    }

    app
}

/// The picker as the app really opens it: the three shipped presets, so the `Declares` column shows
/// both states it has - `—` for the two plain presets, and a count for `demo-ocpp21-full`.
#[test]
fn picker_with_the_shipped_presets() {
    let mut app = App::new(built_in_chargers());

    assert_snapshot(
        "picker_with_the_shipped_presets",
        &render(&mut app, 120, 34),
    );
}

#[test]
fn picker() {
    let mut app = App::new(vec![
        charger_entry(
            "CP001",
            OcppVersion::V16J,
            vec![EvseConfig {
                id: 1,
                connectors: 1,
            }],
        ),
        charger_entry(
            "CP002",
            OcppVersion::V201,
            vec![
                EvseConfig {
                    id: 1,
                    connectors: 2,
                },
                EvseConfig {
                    id: 2,
                    connectors: 1,
                },
            ],
        ),
        charger_entry(
            "CP-2.1",
            OcppVersion::V21,
            vec![EvseConfig {
                id: 1,
                connectors: 1,
            }],
        ),
    ]);

    assert_snapshot("picker", &render(&mut app, 120, 34));
}

/// A charger loaded from a YAML file (its `Source` column names the file, not "built-in") with
/// a remembered CSMS endpoint (its `Last endpoint` column shows the URL, not "—") - the two
/// picker columns Phase 6 added, next to a plain built-in/never-connected charger so both states
/// of each column are visible in the same golden.
#[test]
fn picker_with_configured_charger_and_last_endpoint() {
    let mut app = App::new(vec![
        charger_entry(
            "CP001",
            OcppVersion::V16J,
            vec![EvseConfig {
                id: 1,
                connectors: 1,
            }],
        ),
        ChargerEntry {
            config: charger_config(
                "CP-CUSTOM",
                OcppVersion::V21,
                vec![EvseConfig {
                    id: 1,
                    connectors: 1,
                }],
                false,
            ),
            source: ChargerSource::Configured {
                file_name: "cp-custom.yaml".to_string(),
            },
        },
    ]);
    app.connection_store.remember(
        "CP-CUSTOM",
        ConnectionProfile {
            csms_url: "wss://csms.example.com/CP-CUSTOM".into(),
            ocpp_identity: "CP-CUSTOM".into(),
            security: SecurityProfile::Basic {
                password: String::new(),
            },
        },
    );

    assert_snapshot(
        "picker_with_configured_charger_and_last_endpoint",
        &render(&mut app, 120, 34),
    );
}

#[test]
fn picker_filtered() {
    let mut app = App::new(vec![
        charger_entry(
            "CP001",
            OcppVersion::V16J,
            vec![EvseConfig {
                id: 1,
                connectors: 1,
            }],
        ),
        charger_entry(
            "CP002",
            OcppVersion::V201,
            vec![EvseConfig {
                id: 1,
                connectors: 1,
            }],
        ),
        charger_entry(
            "CP-2.1",
            OcppVersion::V21,
            vec![EvseConfig {
                id: 1,
                connectors: 1,
            }],
        ),
    ]);
    app.picker_filter = TextField::new("cp0");

    assert_snapshot("picker_filtered", &render(&mut app, 120, 34));
}

#[test]
fn picker_no_matches() {
    let mut app = App::new(vec![charger_entry(
        "CP001",
        OcppVersion::V16J,
        vec![EvseConfig {
            id: 1,
            connectors: 1,
        }],
    )]);
    app.picker_filter = TextField::new("zzz");

    assert_snapshot("picker_no_matches", &render(&mut app, 120, 34));
}

#[test]
fn dashboard_idle() {
    let config = charger_config(
        "CP-IDLE",
        OcppVersion::V16J,
        vec![
            EvseConfig {
                id: 1,
                connectors: 1,
            },
            EvseConfig {
                id: 2,
                connectors: 2,
            },
        ],
        false,
    );
    let mut app = dashboard_app(config);

    assert_snapshot("dashboard_idle", &render(&mut app, 120, 34));
}

#[test]
fn dashboard_charging() {
    let mut app = charging_dashboard_app();

    assert_snapshot("dashboard_charging", &render(&mut app, 120, 34));
}

/// The same mid-session dashboard with the connector exporting instead of importing
/// (`docs/hardware-roadmap.md`'s H14): negative power on the meter, the sidebar's direction row,
/// and the export register that keeps accumulating while OCPP's import register freezes. Its own
/// scenario rather than a tweak to `dashboard_charging`, because every one of those is a state the
/// dashboard could only previously have shown by inventing it.
#[test]
fn dashboard_discharging() {
    let mut app = charging_dashboard_app();
    let state = app.charger_state.as_mut().unwrap();
    // What `RunningCharger::set_discharging` plus a tick would have produced, projected the way
    // `apply_hardware_snapshot`/`apply_ocpp_state` project it: direction and the export register on
    // the connector, a *negative* power reading on the meter, and `energy_kwh` (OCPP's import
    // register) frozen at whatever it had already accumulated - it must never run backwards.
    state.evses[0].connectors[0].discharging = true;
    state.evses[0].connectors[0].exported_energy_wh = 2_500;
    state.evses[0].metrics.power_kw = -7.4;
    state.evses[0].metrics.current_a = -7.4 * 1000.0 / 230.0;
    // Long enough for the export to fill the sparkline's window, so the spark shown belongs to the
    // direction the rest of the panel describes.
    state.tick(Duration::from_secs(30));

    assert_snapshot("dashboard_discharging", &render(&mut app, 120, 34));
}

/// The shipped `demo-ocpp21-full` preset on the dashboard: the one charger whose declaration is
/// non-empty out of the box, so this is what the "Declared" strip actually looks like in the app.
/// Built from `built_in_chargers` rather than a hand-written config on purpose - if a capability is
/// added to (or removed from) `SIMULATED_CAPABILITIES` in `core`, this golden is what says so.
#[test]
fn dashboard_declared_capabilities() {
    let demo = built_in_chargers()
        .into_iter()
        .find(|entry| entry.config.id == "demo-ocpp21-full")
        .expect("the full-featured demo preset ships with core");
    let mut app = dashboard_app(demo.config);

    assert_snapshot(
        "dashboard_declared_capabilities",
        &render(&mut app, 120, 34),
    );
}

/// The same declaration on a terminal too narrow to list all of it, which is where the truncation
/// count earns its place: `+N more` rather than a list that quietly stops.
#[test]
fn dashboard_declared_capabilities_narrow() {
    let demo = built_in_chargers()
        .into_iter()
        .find(|entry| entry.config.id == "demo-ocpp21-full")
        .expect("the full-featured demo preset ships with core");
    let mut app = dashboard_app(demo.config);

    assert_snapshot(
        "dashboard_declared_capabilities_narrow",
        &render(&mut app, 80, 24),
    );
}

/// A CSMS connection that failed: the header refusing to repeat the `booting` it was seeded with, and
/// the `Connection` strip holding the reason and the way out. The reason this is a scenario at all is
/// that none of it expires - the previous version of this state was a four-second toast over a header
/// that then claimed the charger was booting indefinitely.
#[test]
fn dashboard_connection_failed() {
    let config = charger_config(
        "CP-2.1",
        OcppVersion::V21,
        vec![EvseConfig {
            id: 1,
            connectors: 1,
        }],
        false,
    );
    let mut app = App::new(vec![]);
    app.screen = Screen::Dashboard;
    // Deliberately *not* `dashboard_app`, which seeds `Connected`: this scenario is about the state a
    // failed dial actually leaves - `Booting`, from `ChargerState::from_config`, with a `LiveCsms`
    // mode naming the CSMS that could not be reached.
    let mut state = ChargerState::from_config(config);
    state.mode = SimulationMode::LiveCsms {
        url: "wss://csms.example.com/CP-2.1".into(),
    };
    app.charger_state = Some(state);
    app.connection_failure = Some("handshake failed: certificate has expired".to_string());
    app.logs.push(crate::logs::LogEntry::error(
        "CSMS connection failed: handshake failed: certificate has expired",
    ));

    assert_snapshot("dashboard_connection_failed", &render(&mut app, 120, 34));
}

/// The command palette on the full-featured preset: protocol commands and hardware actions in one
/// list, which is the whole point of the palette carrying both. The demo charger declares
/// bidirectional power, firmware management and diagnostics, so every kind of row is visible at
/// once; the plain `command_palette` golden next to it shows a charger declaring nothing, where none
/// of them is.
#[test]
fn command_palette_with_hardware_actions() {
    let demo = built_in_chargers()
        .into_iter()
        .find(|entry| entry.config.id == "demo-ocpp21-full")
        .expect("the full-featured demo preset ships with core");
    let mut app = dashboard_app(demo.config);
    // Mid-session on the focused connector, which is what makes the V2G row eligible; and the
    // installer present but idle, as a real snapshot from this charger's bundle would report.
    let state = app.charger_state.as_mut().unwrap();
    Command::PlugInVehicle.apply_to(&mut state.evses[0], 0, "MY-EV-1");
    Command::PresentRfid.apply_to(&mut state.evses[0], 0, "TAG-42");
    app.campaigns = CampaignProgress {
        firmware_install: Some(FirmwareInstallStage::Idle),
        ..Default::default()
    };
    app.handle_key_event(KeyEvent::new(KeyCode::Char('c'), KeyModifiers::NONE));

    assert_snapshot(
        "command_palette_with_hardware_actions",
        &render(&mut app, 120, 34),
    );
}

/// A firmware campaign and a diagnostics log upload in flight at once (`docs/hardware-roadmap.md`'s
/// H10), which is the only thing that puts the campaign strip on screen at all - a charger with no
/// firmware/file-transfer hardware, or one whose installer is idle, spends no rows on it (see
/// `dashboard::body_layout`). Both halves at once on purpose: they are independent campaigns, and
/// the strip claiming to show both is worth pinning.
#[test]
fn dashboard_firmware_campaign() {
    let mut app = charging_dashboard_app();
    app.campaigns = CampaignProgress {
        firmware_install: Some(FirmwareInstallStage::Installing),
        firmware_download: Some(InFlightTransfer {
            elapsed: Duration::from_secs(12),
            duration: Duration::from_secs(20),
            transferred_bytes: 5 * 1024 * 1024,
            total_bytes: 8 * 1024 * 1024,
        }),
        log_upload: Some(InFlightTransfer {
            elapsed: Duration::from_secs(2),
            duration: Duration::from_secs(10),
            transferred_bytes: 400 * 1024,
            total_bytes: 2 * 1024 * 1024,
        }),
    };

    assert_snapshot("dashboard_firmware_campaign", &render(&mut app, 120, 34));
}

/// The log filter prompt open over the dashboard, with the filter already narrowing the pane
/// live as it's typed - the command bar's hints are replaced by the prompt itself.
#[test]
fn dashboard_log_filter_prompt() {
    let mut app = charging_dashboard_app();
    app.handle_key_event(KeyEvent::new(KeyCode::Char('/'), KeyModifiers::NONE));
    for c in "acknowledged".chars() {
        app.handle_key_event(KeyEvent::new(KeyCode::Char(c), KeyModifiers::NONE));
    }

    assert_snapshot("dashboard_log_filter_prompt", &render(&mut app, 120, 34));
}

/// A raised level threshold and a scrolled-up (paused) log pane: both states are only legible
/// from the Logs section title, so this golden pins that title.
#[test]
fn dashboard_log_level_threshold_and_paused() {
    let mut app = charging_dashboard_app();
    // Info -> Debug. Deliberately not raised as far as Warn: that would leave a single
    // matching entry, and a one-entry pane can't be scrolled, so the paused half of this
    // scenario would silently not happen.
    app.handle_key_event(KeyEvent::new(KeyCode::Char('l'), KeyModifiers::NONE));
    app.handle_key_event(KeyEvent::new(KeyCode::PageUp, KeyModifiers::NONE));
    app.handle_key_event(KeyEvent::new(KeyCode::PageUp, KeyModifiers::NONE));

    assert_snapshot(
        "dashboard_log_level_threshold_and_paused",
        &render(&mut app, 120, 34),
    );
}

#[test]
fn dashboard_with_display() {
    let config = charger_config(
        "CP-DISPLAY",
        OcppVersion::V16J,
        vec![EvseConfig {
            id: 1,
            connectors: 1,
        }],
        true,
    );
    let mut app = dashboard_app(config);
    let state = app.charger_state.as_mut().unwrap();
    Command::SetDisplayMessage.apply_to_charger(state, "Welcome to Flowion");

    assert_snapshot("dashboard_with_display", &render(&mut app, 120, 34));
}

#[test]
fn dashboard_faulted() {
    let config = charger_config(
        "CP-FAULT",
        OcppVersion::V16J,
        vec![EvseConfig {
            id: 1,
            connectors: 1,
        }],
        false,
    );
    let mut app = dashboard_app(config);
    let state = app.charger_state.as_mut().unwrap();
    Command::ReportFault.apply_to(&mut state.evses[0], 0, "OverCurrentFailure");

    assert_snapshot("dashboard_faulted", &render(&mut app, 120, 34));
}

#[test]
fn command_palette() {
    let mut app = charging_dashboard_app();
    app.command_palette_open = true;
    app.command_palette_selected = 0;

    assert_snapshot("command_palette", &render(&mut app, 120, 34));
}

#[test]
fn command_palette_filtered() {
    let mut app = charging_dashboard_app();
    app.command_palette_open = true;
    app.command_palette_filter = TextField::new("unplug");

    assert_snapshot("command_palette_filtered", &render(&mut app, 120, 34));
}

#[test]
fn parameter_prompt() {
    let config = charger_config(
        "CP001",
        OcppVersion::V16J,
        vec![EvseConfig {
            id: 1,
            connectors: 1,
        }],
        false,
    );
    let mut app = dashboard_app(config);
    app.parameter_prompt = Some(Command::PlugInVehicle);
    app.parameter_field = TextField::new("MY-EV-1");

    assert_snapshot("parameter_prompt", &render(&mut app, 120, 34));
}

/// The prompt showing its placeholder (empty field) and an inline validation error, the two
/// states Phase 5 added to it.
#[test]
fn parameter_prompt_blank_with_error() {
    let config = charger_config(
        "CP001",
        OcppVersion::V16J,
        vec![EvseConfig {
            id: 1,
            connectors: 1,
        }],
        false,
    );
    let mut app = dashboard_app(config);
    app.handle_key_event(KeyEvent::new(KeyCode::Char('c'), KeyModifiers::NONE));
    app.handle_key_event(KeyEvent::new(KeyCode::Enter, KeyModifiers::NONE)); // opens the prompt
    app.handle_key_event(KeyEvent::new(KeyCode::Enter, KeyModifiers::NONE)); // rejected as blank

    assert_snapshot(
        "parameter_prompt_blank_with_error",
        &render(&mut app, 120, 34),
    );
}

#[test]
fn help_overlay() {
    let config = charger_config(
        "CP001",
        OcppVersion::V16J,
        vec![EvseConfig {
            id: 1,
            connectors: 1,
        }],
        false,
    );
    let mut app = dashboard_app(config);
    app.help_open = true;

    assert_snapshot("help_overlay", &render(&mut app, 120, 34));
}

#[test]
fn quit_confirm() {
    let config = charger_config(
        "CP001",
        OcppVersion::V16J,
        vec![EvseConfig {
            id: 1,
            connectors: 1,
        }],
        false,
    );
    let mut app = dashboard_app(config);
    app.quit_confirm_open = true;

    assert_snapshot("quit_confirm", &render(&mut app, 120, 34));
}

/// An `App` on the connection setup screen for a real V2.1 charger (`charger_state` set, not
/// just the screen enum) so the titled card has an actual charger id to name - the card's whole
/// point is answering "which charger is this," so a scenario that left it `None` would only
/// pin the "charger" placeholder, never the real feature.
fn connection_setup_app() -> App {
    let config = charger_config(
        "CP-2.1",
        OcppVersion::V21,
        vec![EvseConfig {
            id: 1,
            connectors: 1,
        }],
        false,
    );
    let mut app = App::new(vec![]);
    app.screen = Screen::ConnectionSetup;
    app.charger_state = Some(ChargerState::from_config(config));
    app.connection_csms_url = TextField::new("wss://csms.example.com");
    app.connection_ocpp_identity = TextField::new("CP-2.1");
    app.connection_password = TextField::new("secret");
    app.connection_focused_field = 0;
    app
}

#[test]
fn connection_setup() {
    let mut app = connection_setup_app();

    assert_snapshot("connection_setup", &render(&mut app, 120, 34));
}

/// The inline error `App::confirm_connection_setup` shows for a scheme that isn't
/// `ws://`/`wss://`, rather than silently attempting (and failing) a connection.
#[test]
fn connection_setup_invalid_url_error() {
    let mut app = connection_setup_app();
    app.connection_csms_url = TextField::new("https://csms.example.com");
    app.handle_key_event(KeyEvent::new(KeyCode::Enter, KeyModifiers::NONE));

    assert_snapshot(
        "connection_setup_invalid_url_error",
        &render(&mut app, 120, 34),
    );
}

/// Recent URLs from other chargers' remembered profiles, listed under the CSMS URL field.
#[test]
fn connection_setup_url_suggestions() {
    let mut app = connection_setup_app();
    app.connection_csms_url = TextField::default();
    app.connection_store.remember(
        "CP-OTHER-A",
        ConnectionProfile {
            csms_url: "wss://a.example.com".into(),
            ocpp_identity: "CP-OTHER-A".into(),
            security: SecurityProfile::Basic {
                password: String::new(),
            },
        },
    );
    app.connection_store.remember(
        "CP-OTHER-B",
        ConnectionProfile {
            csms_url: "wss://b.example.com".into(),
            ocpp_identity: "CP-OTHER-B".into(),
            security: SecurityProfile::Basic {
                password: String::new(),
            },
        },
    );

    assert_snapshot(
        "connection_setup_url_suggestions",
        &render(&mut app, 120, 34),
    );
}

/// `Ctrl+R` showing the password field's raw value instead of `*`s.
#[test]
fn connection_setup_password_revealed() {
    let mut app = connection_setup_app();
    app.handle_key_event(KeyEvent::new(KeyCode::Char('r'), KeyModifiers::CONTROL));

    assert_snapshot(
        "connection_setup_password_revealed",
        &render(&mut app, 120, 34),
    );
}

#[test]
fn dashboard_narrow() {
    let mut app = charging_dashboard_app();

    assert_snapshot("dashboard_narrow", &render(&mut app, 80, 24));
}

/// A charger with two EVSEs whose connectors are all in different states at once: EVSE 1's
/// first connector charging with a vehicle plugged in, its second connector free, and EVSE 2's
/// only connector faulted. This is exactly the case the old EVSE-strip/single-EVSE-detail UI
/// hid - finding the fault meant Tab-cycling through every EVSE - so the tree is expected to
/// show every connector's status at once without any navigation at all.
fn multi_evse_mixed_status_app() -> App {
    let config = charger_config(
        "CP-MULTI",
        OcppVersion::V16J,
        vec![
            EvseConfig {
                id: 1,
                connectors: 2,
            },
            EvseConfig {
                id: 2,
                connectors: 1,
            },
        ],
        false,
    );
    let mut app = dashboard_app(config);
    let state = app.charger_state.as_mut().unwrap();
    Command::PlugInVehicle.apply_to(&mut state.evses[0], 0, "MY-EV-1");
    Command::PresentRfid.apply_to(&mut state.evses[0], 0, "TAG-1");
    Command::ReportFault.apply_to(&mut state.evses[1], 0, "OverCurrentFailure");
    state.tick(Duration::from_secs(600));
    // See `charging_dashboard_app`'s comment: H3b moved the meter out of `ChargerState::tick`, so
    // this fixture sets EVSE 1's reading directly rather than through a `RunningCharger` it has
    // none of. EVSE 2 is faulted, not charging, so its metrics correctly stay zeroed.
    state.evses[0].metrics = EvseMetrics {
        power_kw: 7.4,
        current_a: 7.4 * 1000.0 / 230.0,
        energy_kwh: 7.4 * 600.0 / 3600.0,
    };
    // Likewise for the hardware-only half (H7): only the charging connector is locked with its
    // contactor closed. The free connector and the faulted one on EVSE 2 stay released and open,
    // which is what their hardware really would report.
    state.evses[0].connectors[0].locked = true;
    state.evses[0].connectors[0].contactor_closed = true;
    app
}

#[test]
fn dashboard_multi_evse_mixed_status() {
    let mut app = multi_evse_mixed_status_app();

    assert_snapshot(
        "dashboard_multi_evse_mixed_status",
        &render(&mut app, 120, 34),
    );
}

/// Focus on a connector belonging to the *second* EVSE - the sidebar must track it there, not
/// stay pinned to EVSE 1's first connector.
#[test]
fn dashboard_focus_second_evse() {
    let mut app = multi_evse_mixed_status_app();
    app.focused = FocusedConnector {
        evse: 1,
        connector: 0,
    };

    assert_snapshot("dashboard_focus_second_evse", &render(&mut app, 120, 34));
}

/// The same mixed-status, multi-EVSE charger as [`dashboard_multi_evse_mixed_status`], but at
/// 80 columns: below the sidebar's width threshold, so the focused connector's detail must
/// appear inline beneath its row in the tree instead of in a sidebar, and every EVSE/connector
/// must still be visible without anything running off the right edge.
#[test]
fn dashboard_narrow_multi_evse() {
    let mut app = multi_evse_mixed_status_app();

    assert_snapshot("dashboard_narrow_multi_evse", &render(&mut app, 80, 24));
}

/// The header's `Local` case: "local simulation" in place of a CSMS URL. Every other
/// dashboard scenario in this file happens to be `Local` too (it's `ChargerState::from_config`'s
/// default), but none of them exists specifically to pin down the header's content - this one
/// does.
#[test]
fn dashboard_header_local() {
    let config = charger_config(
        "CP-LOCAL",
        OcppVersion::V16J,
        vec![EvseConfig {
            id: 1,
            connectors: 1,
        }],
        false,
    );
    let mut app = dashboard_app(config);

    assert_snapshot("dashboard_header_local", &render(&mut app, 120, 34));
}

/// The header's `LiveCsms` case: the CSMS URL takes the mode segment's place. This is the
/// piece of information Phase 2b exists to surface - previously nothing on the dashboard told
/// a user whether their commands reached a real CSMS or just mutated local state.
#[test]
fn dashboard_header_live_csms() {
    let config = charger_config(
        "CP-LIVE",
        OcppVersion::V21,
        vec![EvseConfig {
            id: 1,
            connectors: 1,
        }],
        false,
    );
    let mut app = dashboard_app(config);
    app.charger_state.as_mut().unwrap().mode = SimulationMode::LiveCsms {
        url: "wss://csms.example.com/CP-LIVE".to_string(),
    };

    assert_snapshot("dashboard_header_live_csms", &render(&mut app, 120, 34));
}

/// A connection attempt still in flight (`connect_result_receiver` is `Some`): the header
/// shows an animated spinner and "connecting..." instead of the (still-`Booting`, since the
/// OCPP bridge hasn't reported anything yet) connection status. The spinner frame is derived
/// from `uptime` - fixed here at exactly 500ms of simulated time, deterministically picking
/// the third frame - never from wall-clock time, so this golden can't flake.
#[test]
fn dashboard_header_connecting() {
    let config = charger_config(
        "CP-CONNECTING",
        OcppVersion::V21,
        vec![EvseConfig {
            id: 1,
            connectors: 1,
        }],
        false,
    );
    let mut app = App::new(vec![]);
    app.screen = Screen::Dashboard;
    let mut state = ChargerState::from_config(config);
    state.mode = SimulationMode::LiveCsms {
        url: "wss://csms.example.com/CP-CONNECTING".to_string(),
    };
    state.uptime = Duration::from_millis(500);
    app.charger_state = Some(state);
    let (_result_sender, result_receiver) = tokio::sync::oneshot::channel();
    app.connect_result_receiver = Some(result_receiver);

    assert_snapshot("dashboard_header_connecting", &render(&mut app, 120, 34));
}

#[test]
fn terminal_too_small() {
    let mut app = App::new(vec![]);

    assert_snapshot("terminal_too_small", &render(&mut app, 50, 10));
}
