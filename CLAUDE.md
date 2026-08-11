# CLAUDE.md

This file provides guidance to Claude Code (claude.ai/code) when working with code in this repository.

## Project overview

Flowion Charge Point Simulator (by Flowion AB) is an OCPP charge point simulator. It emulates real
charging hardware over OCPP 1.6J, 2.0.1, and 2.1 so that CSMS backends can be developed and tested
without physical chargers. It is built on top of the `ocpp-charge-point` protocol library (a crates.io
dependency of `charge_point_simulator_core`) and layers fake hardware mappings on top of it to
simulate real charger behavior (connectors, meter values, charging sessions, faults, etc.).
`docs/hardware-roadmap.md` plans how that fake hardware grows to cover the rest of the library's
hardware trait surface.

Charger configurations and mock vehicles are defined via YAML (see the `configuration` example in
README.md). Planned advanced simulation scenarios include plug and charge, vehicle-to-grid (V2G), and
load balancing — keep the core abstractions generic enough to support these later.

## Workspace layout

Cargo workspace with members under `crates/*`:

- `charge_point_simulator_core` — protocol-agnostic simulator logic: fake hardware mapping, charger/vehicle
  state machines, YAML configuration parsing, and OCPP version handling. Anything not specific to a
  particular UI or API belongs here.
- `charge_point_simulator_tui` — interactive `ratatui` dashboard (`crossterm` + `color-eyre`) for monitoring
  and controlling simulated charge points. Depends on `core`; should contain no simulation logic itself.
- A REST API for driving the simulator programmatically will **not** be built in this repository.
  Instead, `charge_point_simulator_core` is published to crates.io, and any REST API is a separate
  downstream consumer of that crate. This means `core`'s public API is a supported, versioned
  surface for external users — treat breaking changes to it accordingly, and keep simulator logic
  in `core` rather than in the TUI so downstream consumers get it too.

Both crates currently share `tokio` via `workspace.dependencies` in the root `Cargo.toml`.

## Development workflow — TDD

This project is developed test-first. For any new behavior in `charge_point_simulator_core` (state
machines, YAML config parsing, OCPP message handling, hardware simulation):

1. Write a failing test first.
2. Implement the minimum code to make it pass.
3. Refactor with tests green.

Don't add functionality without a test driving it. UI code in the `tui` crate that is purely rendering
(layout, widget wiring) is the main exception where TDD is less applicable — but any state transitions
or input handling logic (e.g. `App::handle_key_event`, screen transitions) should still be tested.

## Common commands

```bash
# Build everything
cargo build

# Run the TUI
cargo run -p charge_point_simulator_tui

# Run all tests
cargo test

# Run tests for a single crate
cargo test -p charge_point_simulator_core

# Run a single test by name
cargo test -p charge_point_simulator_core test_name -- --exact

# Lint
cargo clippy --workspace --all-targets

# Format
cargo fmt --all
```

## Notes on current state

`core` implements the charger/EVSE/connector state model, YAML config parsing, the command
model, fake hardware, and a live OCPP 2.1 bridge (`connect_charger` + `apply_ocpp_state`); the
TUI has a working picker → connection setup → dashboard flow.

One thing worth knowing before touching the dashboard:

- `LogBuffer::set_filter`/`clear_filter` are implemented and tested but no key binding reaches
  them yet — that's the source of the workspace's one `dead_code` warning.

A local (unconnected) charger runs a real `ocpp_charge_point::ChargePointRuntime` too (see
`charger::RunningCharger`/`start_local_charger`), just with no CSMS ever dialed and no
`register`/`register_until_accepted` call — `apply_ocpp_state` is the single path into
`ChargerState` for both a local and a live-CSMS charger.

A local charger now reports `connection_status: Offline` permanently. It previously spent
~1.5 simulated seconds `Booting` and was then promoted to `Connected` by `ChargerState::tick`
(`SIMULATED_BOOT_DURATION`); H3b removed that lifecycle along with the rest of `tick`'s
simulation. `Offline` is the honest reading — no CSMS was ever dialed, so `Connected` claimed a
link that did not exist — but it is a deliberate product change, not a bug fix, and the five
tests covering the old boot sequence went with it. An earlier version of this file claimed a
local charger got stuck on `booting` forever; that was already out of date when it was written.
