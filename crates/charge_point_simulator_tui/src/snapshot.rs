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

use crate::app::{App, FocusedConnector};
use crate::logs::{Direction, LogEntry, LogLevel};
use crate::screen::Screen;
use crate::text_field::TextField;
use charge_point_simulator_core::charger::{
    ChargerConfig, ChargerEntry, ChargerSource, ChargerState, Command, ConnectionProfile,
    ConnectionStatus, EvseConfig, OcppVersion, SecurityProfile, SimulationMode,
};
use crossterm::event::{KeyCode, KeyEvent, KeyModifiers};
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
    Command::PlugInVehicle.apply(&mut state.evses[0], "MY-EV-1");
    Command::PresentRfid.apply(&mut state.evses[0], "TAG-42");
    // A fixed, non-wall-clock elapsed time - this mirrors what `App::tick_metrics_with` does to
    // `charger_state` (`state.tick(elapsed)`); `tick_metrics_with` itself is private to the
    // `app` module and out of reach from here, but the rest of what it does
    // (`maybe_send_meter_values`) is a no-op anyway without a live `ocpp_event_sender`, which
    // none of these scenarios set up.
    state.tick(Duration::from_secs(600));

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

#[test]
fn picker() {
    let app = App::new(vec![
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

    assert_snapshot("picker", &render(&app, 120, 34));
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
        &render(&app, 120, 34),
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

    assert_snapshot("picker_filtered", &render(&app, 120, 34));
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

    assert_snapshot("picker_no_matches", &render(&app, 120, 34));
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
    let app = dashboard_app(config);

    assert_snapshot("dashboard_idle", &render(&app, 120, 34));
}

#[test]
fn dashboard_charging() {
    let app = charging_dashboard_app();

    assert_snapshot("dashboard_charging", &render(&app, 120, 34));
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

    assert_snapshot("dashboard_log_filter_prompt", &render(&app, 120, 34));
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
        &render(&app, 120, 34),
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

    assert_snapshot("dashboard_with_display", &render(&app, 120, 34));
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

    assert_snapshot("parameter_prompt", &render(&app, 120, 34));
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

    assert_snapshot("parameter_prompt_blank_with_error", &render(&app, 120, 34));
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

    assert_snapshot("help_overlay", &render(&app, 120, 34));
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

    assert_snapshot("quit_confirm", &render(&app, 120, 34));
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
    let app = connection_setup_app();

    assert_snapshot("connection_setup", &render(&app, 120, 34));
}

/// The inline error `App::confirm_connection_setup` shows for a scheme that isn't
/// `ws://`/`wss://`, rather than silently attempting (and failing) a connection.
#[test]
fn connection_setup_invalid_url_error() {
    let mut app = connection_setup_app();
    app.connection_csms_url = TextField::new("https://csms.example.com");
    app.handle_key_event(KeyEvent::new(KeyCode::Enter, KeyModifiers::NONE));

    assert_snapshot("connection_setup_invalid_url_error", &render(&app, 120, 34));
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

    assert_snapshot("connection_setup_url_suggestions", &render(&app, 120, 34));
}

/// `Ctrl+R` showing the password field's raw value instead of `*`s.
#[test]
fn connection_setup_password_revealed() {
    let mut app = connection_setup_app();
    app.handle_key_event(KeyEvent::new(KeyCode::Char('r'), KeyModifiers::CONTROL));

    assert_snapshot("connection_setup_password_revealed", &render(&app, 120, 34));
}

#[test]
fn dashboard_narrow() {
    let app = charging_dashboard_app();

    assert_snapshot("dashboard_narrow", &render(&app, 80, 24));
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
    app
}

#[test]
fn dashboard_multi_evse_mixed_status() {
    let app = multi_evse_mixed_status_app();

    assert_snapshot("dashboard_multi_evse_mixed_status", &render(&app, 120, 34));
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

    assert_snapshot("dashboard_focus_second_evse", &render(&app, 120, 34));
}

/// The same mixed-status, multi-EVSE charger as [`dashboard_multi_evse_mixed_status`], but at
/// 80 columns: below the sidebar's width threshold, so the focused connector's detail must
/// appear inline beneath its row in the tree instead of in a sidebar, and every EVSE/connector
/// must still be visible without anything running off the right edge.
#[test]
fn dashboard_narrow_multi_evse() {
    let app = multi_evse_mixed_status_app();

    assert_snapshot("dashboard_narrow_multi_evse", &render(&app, 80, 24));
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
    let app = dashboard_app(config);

    assert_snapshot("dashboard_header_local", &render(&app, 120, 34));
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

    assert_snapshot("dashboard_header_live_csms", &render(&app, 120, 34));
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

    assert_snapshot("dashboard_header_connecting", &render(&app, 120, 34));
}

#[test]
fn terminal_too_small() {
    let app = App::new(vec![]);

    assert_snapshot("terminal_too_small", &render(&app, 50, 10));
}
