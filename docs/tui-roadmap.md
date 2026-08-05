# TUI redesign roadmap

A staged plan for making the dashboard legible and modern. Phases 0–3 have landed on
`tui-redesign-phase-0-1`; what follows them is described here in enough detail to pick up cold.

The guiding principles, which every remaining phase should be checked against:

| Principle | What it means here |
| --- | --- |
| Density over decoration | One-line section headers with dim rules, not four-sided boxes. |
| Everything at a glance | Focus adds detail; it never reveals something for the first time. |
| Color means something | Dim neutral chrome; status colors reserved for status. |
| Truth in the status bar | Never display a state the simulator is not actually in. |
| The log is the product | For an OCPP tool, the protocol trace is the thing being sold. |
| Contextual affordances | Hints change with mode; nothing permanently hides them. |

## Done

- **Phase 0** — golden-file snapshot harness (`src/snapshot.rs`, `snapshots/`), and all rendering
  extracted from `app.rs` into `src/ui/`, verified byte-identical by the goldens.
- **Phase 1** — semantic theme vocabulary (`theme.rs`), panels converted from boxes to top-rule
  sections, three reclaimed rows given to the log pane.
- **Phase 2** — `SimulationMode` fixes a charger that displayed `booting` forever, and the header
  states whether commands drive a real CSMS or local state.
- **Phase 3** — per-connector command targeting, session duration, SoC progression, and the EVSE
  tree with detail sidebar.

## Phase 4 — the log pane as a protocol trace

The highest-value work remaining. Today every line reads
`[INFO] charge_point_simulator_core::charger::state: heartbeat sent` — roughly 60 columns of
module path before the message, no timestamps, no level colors, and no indication of which
direction a message travelled.

- Replace `LogBuffer`'s `Vec<String>` with a structured entry: timestamp, level, target, message,
  fields, optional direction (inbound/outbound), optional OCPP action name. `tracing_bridge.rs`
  populates the structure instead of pre-formatting a string.
- Render as columns with elided targets (`charge_point_simulator_core::charger::state` →
  `core::state`), level colors from the theme's severity tier, and `→`/`←` direction markers.
- **Wire up the filter that already exists.** `LogBuffer::set_filter`/`clear_filter` are
  implemented and tested but no key binding reaches them — this is the source of the workspace's
  only `dead_code` warning. Bind `/` to open the filter and `Esc` to clear it.
- Add a level threshold, cycled with a key.
- Bound the buffer (ring, ~5000 entries) and cache the filtered view. `visible_lines` currently
  calls `filtered()`, allocating a `Vec` of every entry, on every frame, and the buffer grows
  without limit for the life of the session.
- `g`/`G` for top and bottom, and a `Scrollbar` showing real position. The "paused" state is
  currently conveyed only by the panel title.

## Phase 5 — command palette, feedback, and help

- Rebind the palette to `Ctrl+K`, keeping `c` as an alias. Fuzzy subsequence matching rather than
  the current plain substring match on the label, plus per-command descriptions.
- Show the resolved target connector in each palette row before the command is committed, and
  allow retargeting from inside the palette.
- Parameter prompt: label, placeholder, per-parameter history (last vehicle id, last RFID tag),
  and inline validation.
- **Toasts must expire.** `status_message` currently persists indefinitely, permanently hiding the
  keybinding hint line. Give it a timestamp and clear it after a few seconds. Use an injectable
  clock, following the pattern already established by `tick_metrics_with`.
- **Fix the help overlay**, which does not fit its own content: the popup is sized
  `min(56, 14)` — 12 usable rows — against 15 lines of text, so the last entries never render.
  Size it to its content and make it scrollable. Since Phase 1b gave the log pane more height,
  the popup also now overlaps the Logs section rule; sizing it properly resolves both.
- Derive the help text from a single keybinding table, so help cannot drift from behavior again.

## Phase 6 — picker and connection setup

- Picker as an aligned table with columns, surfacing `ChargerSource` (built-in vs which YAML file)
  and the last-used endpoint from `ConnectionStore`, so Enter's consequences are visible.
- Connection setup: a titled card naming the charger, URL scheme validation (`ws://`/`wss://`),
  a password reveal toggle, recent-URL suggestions, and inline errors rather than a bare status
  line.

## Phase 7 — polish

- Mouse support: click to focus a connector, wheel-scroll the log, click palette rows.
- Scrollbars on the tree and log panes; `Ctrl+L` to clear logs; copy the focused log line.
- A heartbeat pulse in the header, so "alive but idle" is distinguishable from "hung".
- **Fix the command bar hint truncation.** At 80 columns the hint is cut mid-word, losing "help"
  from the very line that advertises it. It needs to wrap or shorten by priority, the way the
  header's segments already do.

## Open decisions

These need a human call and are deliberately not settled:

1. **EVSE summary status priority.** An EVSE row shows one status, but core models status only
   per connector. It is currently derived as
   `faulted > charging > occupied > reserved > unavailable > available`. Whether `reserved` should
   outrank `occupied` is a domain judgement.
2. **Power sparkline ownership.** A sparkline needs rolling metrics history. Belongs in core
   (where a future REST API could also consume it) or TUI-side as pure presentation?
3. **Deprecating the first-eligible command API.** `Command::apply`/`is_available` and
   `build_ocpp_event` are still `pub` and tested, but nothing calls them now that the TUI targets
   a specific connector. Keep as library surface, or remove?
4. **Charging behavior at 100% SoC.** A full vehicle keeps drawing full simulated power
   indefinitely. Tapering or stopping is charging-strategy behavior and belongs with the planned
   scenario work, not the meter tick.

## Known gaps not yet scheduled

- **No OCPP message counters.** The header has no traffic indicator because
  `ChargePointState` exposes no sent/received counts. The quantities available TUI-side (commands
  dispatched, state snapshots received) are not message counts, and displaying them under that
  label would misrepresent them. Needs support in `ocpp-charge-point` upstream.
- **Lock and contactor state are unreachable.** `FakeConnector` tracks both, but they live in the
  OCPP hardware layer rather than on `ChargerState`, so the sidebar cannot show them without new
  plumbing.
- **The view model is thin.** `DashboardView` borrows `&ChargerState` wholesale rather than
  reshaping it, so render functions still walk nested state. Adequate for one frontend; worth
  revisiting when the planned REST API becomes a second consumer.

## Working agreements

- The snapshot goldens under `crates/charge_point_simulator_tui/snapshots/` are the regression net
  for all rendering work. Regenerate with
  `UPDATE_SNAPSHOTS=1 cargo test -p charge_point_simulator_tui`, but never blindly — inspect each
  regenerated golden and confirm the new output is actually better.
- A pure refactor must leave the goldens byte-identical. If one changes, rendering changed.
- Goldens capture text only, not styles, so color changes do not churn them.
- Scenarios must be deterministic: drive simulated time with a fixed `elapsed`, never a wall
  clock. This is also why the connecting spinner animates from `uptime`.
- Beware accumulators whose storage is coarser than their per-step increment. SoC progression was
  silently inert in the running app because a `u8` discarded the fraction each ~100ms tick added,
  while tests feeding one large `elapsed` passed. Test at the cadence the app really uses.
