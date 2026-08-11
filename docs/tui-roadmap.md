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

- **Phase 8 — catching up with the hardware layer.** Waves H4–H14 of `docs/hardware-roadmap.md`
  landed a great deal of hardware the dashboard could not see, could not reach, or was never given.
  This phase closes all three, and is why the "Lock and contactor state are unreachable" gap is no
  longer listed below.

  **The thread boundary was the blocker, and it is now `core`'s to solve.** H7's
  lock/contactor/current-limit trio is written by `apply_hardware_state`, which needs the hardware
  handle — private to `RunningCharger`, which is not `Send` and lives on the charger's own thread. So
  the TUI could never have called it. `core` gained `ConnectorHardwareSnapshot`,
  `ocpp_bridge::hardware_snapshot`/`apply_hardware_snapshot` and
  `RunningCharger::hardware_snapshot`, splitting that projection in two at the point where the value
  stops needing the hardware; `apply_hardware_state` is now defined in terms of them, so the two
  paths cannot drift. The TUI's channel carries a `ChargerSnapshot { ocpp, hardware, campaigns }`
  instead of a bare `ChargePointState`, and applying both halves in `drain_charger_snapshots` is
  exactly what `RunningCharger::apply_state` does on the other side — asserted in `core`'s
  `a_hardware_snapshot_carries_what_apply_state_would_have_written`. Deliberately in `core`, not the
  TUI: any downstream consumer of the published crate with a request/response boundary hits the
  identical wall.

  `ConnectorState` also gained `discharging`/`exported_energy_wh` (H14), filled by the same
  projection, since `MeterSample` carries no export figure at all and OCPP's `energy_wh` deliberately
  freezes rather than running backwards while exporting.

  **Snapshots are now published on ticks and hardware actions too**, not only `states.changed()`:
  the hardware half has no `ChargePointState` counterpart, so nothing about it is guaranteed to bump
  the state version, and the export register in particular rises every tick with the protocol state
  untouched.

  **What reached the screen.** The sidebar states lock, contactor and current limit unconditionally
  (`released`/`open`/`none` is as much an answer as its opposite), plus a direction row while
  exporting and the export register whenever it is non-zero; `Some(0)` renders as
  `suspended (0.0 A)` so a suspending profile can't be mistaken for the absence of one. The narrow
  inline line carries only the limit and a `V2G` marker — it already runs to ~66 columns and has to
  survive at 80 — leaving lock and contactor to the sidebar, exactly as it already leaves current and
  energy there.

  **`d` toggles discharge**, through a new `HardwareControl` channel to the charger's thread calling
  `RunningCharger::set_discharging`. Its own channel and its own type, deliberately, because no OCPP
  message can carry a direction (`HardwareCommand` has six variants and none of them can) — keeping
  it out of the `ChargePointEvent` path is what stops the control reading as something the protocol
  did. Gated on the charger declaring `supports_bidirectional_power` and on a vehicle actually being
  plugged in: the hardware would obey regardless, but the declaration is the only thing the CSMS can
  see, and direction with nothing connected is a reading no real charger produces. The current
  direction is read back off the last snapshot rather than a local flag, so the toggle can never
  disagree with the screen.

  **The bundle was the other half of the gap.** `App` passed `ChargerHardware::new` — storage and a
  display — so `firmware_installer`, `firmware_verifier`, `file_transfer` and `certificate_store`
  were always `None` and a charger declaring `firmware_management`/`diagnostics`/
  `certificate_management` registered *nothing*. `app.rs::charger_hardware` now builds each piece
  the charger declares, with the install duration and transfer profiles as named constants carrying
  their rationale. `key_store` stays `None` on purpose: nothing registers a `KeyStore`, so supplying
  one would only suggest the charger does something with it.

  Progress is surfaced through a `Firmware & files` strip, fed by `CampaignHandles` — `Arc` clones
  taken out of the bundle *before* it is consumed, the same "clone before handing ownership away"
  pattern `ChargerHardware`'s doc comment prescribes. This needed one observational accessor in
  `core`: `FakeFileTransfer::download_in_flight`/`upload_in_flight` (returning `InFlightTransfer`),
  because a transfer's progress otherwise goes only to upstream's callback and on to the CSMS, where
  no frontend can see it. `Some(FirmwareInstallStage::Idle)` versus `None` is a real distinction: an
  installer with nothing to do, versus a charger that cannot install firmware.

  **A `Declared` strip** lists what the charger claims, which is the answer to both "why did that
  refuse?" and "what is my CSMS being told?". Truncates with `+N more` rather than stopping quietly.
  `has_display` is the one declaration left out — it has a whole section of its own two rows above.
  All three strips are conditional and share one decision point, `dashboard::body_strips_for`, so
  `render` and mouse hit-testing cannot lay the body out differently for the same state.

  **`core` ships `demo-ocpp21-full`**, a preset declaring `SIMULATED_CAPABILITIES` — every capability
  with hardware behind it and no others, with a test that keeps that constant honest — so none of the
  above needs hand-written YAML to reach.

- **Phase 9 — the actions the hardware could take but nothing could ask for.** Phase 8 gave the
  charger firmware, file-transfer and certificate hardware and put its progress on screen, but left
  three things unfinished, all now closed.

  **Local campaigns are drivable.** `register_optional_hardware` gates `firmware_updates`/
  `log_uploads` on `has_csms`, so in local mode the installer and file transfer existed, were ticked,
  and could never be asked to do anything — the `Firmware & files` strip could only ever animate
  against a real CSMS. `core` gained inherent, CSMS-free entry points on the fakes themselves
  (`FakeFirmwareInstaller::run_install`, `FakeFileTransfer::run_download`/`run_upload`), so the TUI
  drives them without importing upstream's traits — the boundary its `Cargo.toml` states, where
  `ocpp-charge-point` is a dev-dependency only. `HardwareControl` grew `InstallFirmware`,
  `UploadDiagnostics` and the three failure arms, and the charger thread *spawns* a campaign rather
  than awaiting it, so a 30-second install doesn't queue every later control behind it. The spawned
  task holds only `Arc` clones of the hardware, never `RunningCharger` (which isn't `Send`).

  **The palette carries both kinds of action.** A new `src/actions.rs` holds `HardwareAction` and a
  `PaletteEntry` enum; `App::palette_entries` lists eligible commands then eligible hardware actions,
  fuzzy-matched together. They stay distinct types in distinct channels because a `Command` becomes a
  `ChargePointEvent` and a `HardwareAction` has no protocol path at all — keeping them apart is what
  stops the second reading as something OCPP did. Availability is gated on the charger's declaration,
  so an undeclared action is absent rather than greyed out, and the palette's target line now follows
  the *selected* entry: `EVSE 1 / C1  (Tab to retarget)` for a connector-scoped one, the charger's id
  and `(whole charger)` for a firmware update. `d` stays as the discharge shortcut and delegates to
  the same action, keeping its refusal messages (a keybinding needs them; the palette never lists an
  unavailable row).

  Failure arming is labelled "from now on", not "next": the fakes' flags are armed once and never
  cleared, and the nicer sentence would describe behavior the simulator doesn't have.

  **The picker shows what a charger declares** — a `Declares` column holding a count, because the
  shipped `demo-ocpp21-full` alone declares ten and any list short enough for a table cell would be a
  list of the first two. The `Charger` column moved to `Fill(3)` to pay for it: an id truncated past
  the part that distinguishes it makes the whole row useless.

  **Settled decision 2 is implemented** — see it below for the sampling design and why the sparkline
  plots magnitude.

  One more thing this turned up, and fixed: `App::apply_command`'s fallback to `Command::apply_to`.
  It was only reachable before the charger's first snapshot arrived, and in that window it mutated
  `ChargerState` directly — the change looked like it worked and was then silently reverted by the
  snapshot. It now reports "not ready yet", which also retires H3b's "local mode is half-converged"
  note. `Command::apply_to` stays in `core` as published API; nothing in the TUI calls it.

- **Phase 10 — a failed CSMS connection that stays failed.** A failed dial used to be a four-second
  toast, over a header that then reported `booting` indefinitely — `connection_status` is written only
  by `apply_ocpp_state`, the connection thread has exited, and no snapshot will ever arrive to correct
  it. So the one state on this screen that nothing else will ever mention again was also the one that
  expired fastest, and the header actively contradicted it.

  `App::connection_failure` holds the error until something is genuinely done about it (a retry, a
  successful connection, or leaving for the picker). `dashboard::LinkState` replaces
  `header_segments`' `connecting: bool` with three states, so the header shows `✕ connection failed`
  rather than a stale status — and a retry in flight outranks the failure it is retrying, since the
  spinner is then the live answer. A `Connection` strip (three rows: rule, reason, next step) carries
  the CSMS error verbatim, truncated visibly with `…` when it must be, because a URL typo, an expired
  certificate and a rejected password are three different problems and the words are what separate
  them. `r` reopens connection setup with every field prefilled from the profile that failed, so
  fixing a typo is an edit; it deliberately does nothing when there is no failure, since a connected
  charger must not be torn down by a stray keypress. The failure is also pushed to the log at `Error`
  level via a new `LogEntry::error` — `Info` is what the level threshold hides first, and this is the
  line that explains everything else on screen.

## Where we are

Phases 0–10 have all landed; see the Done section above. Nothing is scheduled after this — the open
decisions below are all settled, so the next move is one of the unscheduled gaps after them. (No REST
API crate is coming here — `core` is published to crates.io and any REST API lives in a separate
downstream repo. See `CLAUDE.md`.)

## Settled decisions

All four former open decisions have had their human call:

1. **EVSE summary status priority** — `occupied` outranks `reserved`. A car physically present is
   the more actionable fact than a booking against a connector that may still be empty. The
   existing `faulted > charging > occupied > reserved > unavailable > available` order in
   `dashboard.rs::evse_summary_status` already matched, so nothing changed but the doc comment,
   which now says this is deliberate.
2. **Power sparkline ownership** — rolling metrics history belongs in `core`, not the TUI. A
   sparkline is then pure presentation over data downstream consumers of the published `core`
   crate can serve too. **Implemented in Phase 8**, exactly as described: `core`'s `PowerHistory`
   lives beside `EvseMetrics` on `EvseState` and is driven by `EvseState::tick`'s injected
   `elapsed`, never a wall clock. It *samples* rather than recording every tick — one value per
   simulated second, 60 kept — because recording per tick would make the window's span depend on the
   frame rate, and would hold about six seconds of history at the TUI's ~100ms cadence. The TUI
   draws the last 24 of those samples in the sidebar. Magnitude only: eight block glyphs cannot show
   a signed series against a baseline, and an exporting EVSE's samples are negative (H14) — the sign
   is stated by the direction row and the signed `kW` figure beside the spark instead.
3. **Deprecating the first-eligible command API** — removed. `Command::apply`/`Command::is_available`
   and `ocpp_bridge::build_ocpp_event` are gone, along with `build_ocpp_event`'s re-export from
   `charger/mod.rs`. Every caller already targeted a specific connector; keeping a second
   "whichever connector comes first" entry point was an invitation to act on a connector the user
   isn't looking at. The tests that covered them were rewritten against
   `is_available_for_connector`/`apply_to` rather than deleted, so per-command eligibility and
   mutation coverage is unchanged; the two tests that only asserted the two APIs agreed with each
   other were dropped. Goldens are byte-identical.
4. **Charging behavior at 100% SoC** — accepted as-is for now. A full vehicle continuing to draw
   simulated power is a known inaccuracy, deliberately left to the planned advanced vehicle
   capabilities (tapering, V2G, charging strategies) rather than patched into the meter tick.

## Known gaps not yet scheduled

- **No OCPP message counters.** The header has no traffic indicator because
  `ChargePointState` exposes no sent/received counts. The quantities available TUI-side (commands
  dispatched, state snapshots received) are not message counts, and displaying them under that
  label would misrepresent them. Needs support in `ocpp-charge-point` upstream.
- **The view model is thin.** `DashboardView` borrows `&ChargerState` wholesale rather than
  reshaping it, so render functions still walk nested state. Adequate for one frontend; worth
  revisiting if a second frontend appears — though as an out-of-repo consumer of the published
  `core` crate, that frontend would build its own view model anyway.

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
