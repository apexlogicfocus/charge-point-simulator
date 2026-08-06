# TUI redesign roadmap

A staged plan for making the dashboard legible and modern. Phases 0–6 have landed on
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
- **Phase 4** — the log pane as a protocol trace: a structured `LogEntry` (timestamp, level,
  target, message, fields, direction, OCPP action) replacing `Vec<String>`, populated by
  `tracing_bridge.rs`; column rendering with elided targets and `→`/`←` markers; `/` filter,
  `l` level threshold, `g`/`G`, and a real scrollbar; a 5000-entry ring with a cached filtered
  view. The workspace's `dead_code` warning is gone — the filter now has a key binding.

  The help-overlay sizing fix listed under Phase 5 was pulled forward: this phase added four
  bindings to the bottom of a popup that was clipping its own last three lines, so they would
  have been documented somewhere the user could never see. `render_help` now sizes to its
  content. The rest of Phase 5 is untouched.
- **Phase 5** — `Ctrl+K` opens the palette (`c` kept as an alias), matched by fuzzy subsequence
  (`src/fuzzy.rs`) with per-command descriptions and the resolved target connector shown on
  every row, retargetable in place with `Tab`. The parameter prompt gained an example
  placeholder, per-parameter history (prefilled, so repeating a command is just Enter), and
  inline validation. Status messages expire after 4s via `expire_status_message`, following
  `tick_metrics_with`'s injectable-clock pattern — they had been hiding the keybinding hint
  permanently. The help overlay is scrollable and, with the dashboard's hint line, derived from
  a single `src/keybindings.rs` table.
- **Phase 6** — the picker is a `ratatui::widgets::Table` with Charger/OCPP/EVSEs/Source/Last
  endpoint columns; `ChargerSource::Configured` now carries the YAML file's bare name for the
  Source column, and `ConnectionStore::recent_urls` backs the Last endpoint column (and the
  connection setup screen's suggestions, below). Column text is truncated with a trailing `…` at
  each column's real rendered width (`picker.rs::column_widths` replicates
  `Table::get_column_widths` exactly) rather than left for `Table` to clip silently mid-word.

  Connection setup gained a titled card (`Connect <charger id> to a CSMS`, via
  `theme::bordered_block`) around the three fields; a CSMS URL scheme check
  (`app.rs::validate_csms_url`) that blocks `Enter` with an inline error under the field, on
  the same "always-allotted row, cleared when edited" pattern as the parameter prompt's error;
  `Ctrl+R` to reveal the password field's raw value; and `PageUp`/`PageDown` to cycle the URL
  field through every remembered CSMS URL (not just the current charger's), narrowing as you
  type. The help overlay gained a "Connection setup" section it never had before, and pushed
  the overlay's line count past what fits at 120x34 uncompressed — `render_help` now reserves a
  one-row margin top and bottom so a full-height popup doesn't sit flush against (and visually
  merge with) the header and command bar; past that it's the existing scrollbar.
- **Phase 7 (hint truncation)** — `keybindings::dashboard_hint_for_width` replaces the fixed hint
  string with a priority table, widening tiers until the joined line fits. At 80 columns the bar
  no longer cuts mid-word, and `?: help` — priority 3, never dropped — survives on the very line
  that advertises it. Same tiered-fallback shape as `dashboard.rs::header_segments`.
- **Phase 7 (heartbeat pulse)** — `dashboard::heartbeat_pulse_frame` breathes a dot (`· ○ ● ○`)
  every 300ms of simulated `uptime`, so a live-but-idle simulator reads differently from a hung
  one. Driven by `uptime` (which only advances via `ChargerState::tick`) rather than wall clock,
  keeping snapshots deterministic; visually distinct from the connecting spinner. It rides in the
  same header tier as uptime and is dropped with it — a moving dot means nothing without the
  uptime it vouches for. There is deliberately no OCPP-heartbeat claim here: `ChargerState`
  exposes no heartbeat traffic (see "No OCPP message counters" below), and showing invented
  protocol data would violate truth in the status bar.
- **Phase 7 (tree scrollbar, clear, copy)** — a shared `render_scrollbar` helper now serves both
  the log pane and the EVSE tree, whose scroll offset keeps the focused connector visible.
  `Ctrl+L` clears the log buffer (`LogBuffer::clear`); `y` copies the focused entry via
  `LogEntry::to_plain_text` and the new `clipboard.rs` (backed by `arboard` — the app already
  assumes a real terminal, so OSC 52's remote-friendliness bought nothing). Copy failures surface
  as a status message, never a panic. Both bindings sit at the lowest hint priority, so they are
  the first to drop on a narrow terminal.
- **Phase 7 (mouse support)** — `main.rs` enables crossterm mouse capture around `ratatui::init`/
  `restore` (not `ratatui::run`, which offers no hook for it), disabling it again on both the
  normal exit path and the panic path by re-chaining `ratatui::try_init`'s own panic hook.
  `App::handle_mouse_event` hit-tests clicks/wheel events against `App::last_frame_area`, the
  size `App::draw` stashes from the last real frame, rather than re-querying the terminal (which
  has no size to query in tests). Clicking a connector row in the EVSE tree focuses it, via a new
  `dashboard::tree_line_to_connector` (the reverse of the existing `tree_focus_line_index`) and
  `dashboard::content_row_at`, both pure `Rect`/line-index math, unit-tested without a terminal.
  The mouse wheel scrolls the log pane when the cursor is over it, reusing the Phase 4
  `LogBuffer::scroll_up`/`scroll_down` (so it pauses/resumes exactly like `PageUp`/`PageDown`
  already did). Clicking a command palette row moves the selection to it — the same as `↑`/`↓` —
  without dispatching; `Enter` is still what runs a command, so a misclick on a state-mutating
  command can't fire it. `palette.rs` gained `palette_popup_rect`/`palette_list_area`/
  `command_index_at`, factored out of `render_command_palette`'s inline layout so rendering and
  hit-testing can never disagree about where a row is. Pure refactor for rendering: every golden
  is byte-identical.

## Phase 7 — polish

All four items have landed; see the Done section above. Nothing is scheduled after this — the
next move is one of the open decisions below, or the unscheduled gaps after them.

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
