# CLAUDE.md

This file provides guidance to Claude Code (claude.ai/code) when working with code in this repository.

## Project overview

Flowion Charge Point Simulator (by Flowion AB) is an OCPP charge point simulator. It emulates real
charging hardware over OCPP 1.6J, 2.0.1, and 2.1 so that CSMS backends can be developed and tested
without physical chargers. It is built on top of the `ocpp-charge-point` protocol library (see the
commented-out git dependency in `crates/charge_point_simulator_core/Cargo.toml`) and layers fake
hardware mappings on top of it to simulate real charger behavior (connectors, meter values, charging
sessions, faults, etc.).

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
- A proprietary REST API crate for driving the simulator programmatically is planned but not yet present.
  When it's added, it should follow the same pattern: thin interface layer over `core`.

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

The codebase is in early scaffolding: `charge_point_simulator_core/src/lib.rs` is empty, and
`App::draw` in the TUI is `todo!()`. There is no simulation, OCPP, or YAML config logic implemented yet
— when adding the first real features, establish the core module structure (e.g. hardware model,
charger/vehicle config, OCPP version abstraction) deliberately, since later code will build on it.
