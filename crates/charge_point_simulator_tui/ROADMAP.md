# TUI Roadmap

This describes the planned UX and implementation phases for `charge_point_simulator_tui`. It builds on
the `Screen` enum already scaffolded in `src/screen.rs` (`PickCharger` → `Dashboard`).

## Screens

### 1. Charger Picker (`Screen::PickCharger`)

Shown on startup. A single scrollable list combining:

- **Built-in chargers** — a small set of preset configs compiled into the binary, useful for quick
  demos/tests without any YAML.
- **Configured chargers** — one entry per charger definition found in the user's YAML config
  directory (path convention TBD, e.g. `./chargers/*.yaml` or `$FLOWION_CONFIG_DIR`).

Each row shows charger id, OCPP version, and EVSE count. Selecting one (Enter) loads that config and
transitions to `Screen::Dashboard`. List should support `j/k`/arrow navigation and a `/`-style filter
for setups with many configured chargers.

### 2. Dashboard (`Screen::Dashboard`)

Split into fixed regions:

| Block | Content |
|---|---|
| **Overview** | Charger id, OCPP version, CSMS connection status (connected/reconnecting/offline), boot state, uptime, last heartbeat |
| **Log stream** | Scrolling OCPP/protocol log (outgoing/incoming messages, internal events), newest at bottom, autoscroll with a "scrolled up, paused" indicator |
| **EVSE panels** | One panel per EVSE showing its connectors, connector status, connected vehicle (if any), and live metrics (power, current, energy, SoC if known) |
| **Command bar** | Always-visible hint line + modal command palette for issuing simulated events |

Suggested layout: overview as a thin header row, EVSE panels as the main body, log stream docked at the
bottom (collapsible/resizable), command bar as a footer line that expands into a modal on activation.

## Handling many EVSEs

A charger can have anywhere from 1 to dozens of EVSEs, each with multiple connectors — the main body
can't just stack full detail panels. Plan:

- **Compact overview strip**: every EVSE gets a small badge (id + color-coded connector status dots).
  This always fits, even for large chargers, by wrapping or scrolling horizontally.
- **Focused detail panel**: one EVSE is "focused" at a time; its full detail (connectors, connected
  vehicle, live metrics) renders in the main panel below the strip. Arrow keys / number keys / `Tab`
  move focus between EVSEs; the strip highlights the focused badge.
- Fall back to this master-detail approach uniformly rather than branching UI behavior on EVSE count —
  it degrades gracefully from 1 EVSE (strip is trivial, detail takes the whole body) to many (strip
  scrolls, detail still shows one at a time).
- Revisit later if users want simultaneous multi-EVSE detail (e.g. a resizable grid), but don't build
  that until the single-focus version is in use.

## Command system

Commands simulate real-world events a physical charger would react to: vehicle plugged in/out, RFID
badge presented, connector fault injected, meter value pushed, remote start/stop from CSMS, etc.

- Triggered by a dedicated key (e.g. `:` or `c`) opening a modal command palette, similar to command
  palettes in editors — type to fuzzy-filter available commands.
- Some commands are contextual to the focused EVSE/connector (e.g. "plug in vehicle" only makes sense
  for a connector that's currently unplugged) — the palette should filter by what's valid given current
  state.
- Commands needing parameters (vehicle profile, RFID tag id, fault code) prompt for them via a small
  form after selection, rather than requiring raw argument syntax.
- Every dispatched command is echoed into the log stream so the log is a complete session record.
- Common commands can additionally get direct keybindings (documented in a help overlay, e.g. `?`) once
  the palette is stable, so frequent actions don't require the full modal.

## Implementation phases

1. **Screen state machine** — formalize navigation between `PickCharger` and `Dashboard` in `App`
   (currently `App::draw` is `todo!()`). Test-first: `handle_key_event`/selection transitions, no
   rendering assertions needed here.
2. **Charger picker, static** — render the combined built-in + YAML-discovered list; wire selection to
   switch screens. YAML loading and built-in preset list live in `charge_point_simulator_core` and are
   TDD'd there; the TUI just renders what core returns.
3. **Dashboard static layout** — lay out overview/log/EVSE-strip/command-bar regions with placeholder
   content, using `ratatui::layout::Layout`. Confirms the layout holds up before wiring real data.
4. **Live data wiring** — connect dashboard widgets to the simulator's charger/EVSE state once `core`
   exposes it (state transitions and data mapping are TDD'd in `core`; the TUI subscribes/renders).
5. **Log stream behavior** — scrolling buffer with autoscroll + "paused while scrolled up" state,
   optional filter. State logic (buffer, scroll offset, filter matching) is unit-tested.
6. **EVSE strip + focus navigation** — implement the compact-strip/focused-detail pattern above; focus
   movement logic is unit-tested independent of rendering.
7. **Command palette** — fuzzy filter, contextual availability, parameter forms, dispatch into core,
   echo into log. Filtering/validity/dispatch logic is TDD'd; the modal chrome is not.
8. **Polish** — status color scheme, help overlay, resize handling, confirmation on quit, toast/error
   surface for failed commands.

Per `CLAUDE.md`, all state/logic in each phase (transitions, filtering, focus movement, command
dispatch) is written test-first; only pure rendering code is exempt.
