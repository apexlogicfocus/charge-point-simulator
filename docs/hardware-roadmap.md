# Hardware layer roadmap

A staged plan for extending the simulator's fake hardware from the three base traits it
implements today to the full hardware surface `ocpp-charge-point` exposes, so a CSMS can be tested
against smart charging, firmware campaigns, plug and charge, and V2G without physical chargers.

Written to be picked up cold, and deliberately structured for parallel work: tasks own disjoint
files wherever possible, and the dependency table at the end says what can run at the same time.

The guiding principles, which every task should be checked against:

| Principle | What it means here |
| --- | --- |
| One simulation, not two | The fake hardware owns the physics; `ChargerState` projects it. |
| Never advertise what isn't simulated | A `Capabilities` flag goes `true` only once hardware backs it. |
| Simulated time, never wall clock | Every tick takes an injected `elapsed`, as the TUI already requires. |
| `Ok` means the hardware moved | Upstream's sharpest contract — a fake that lies fails open. |
| `core` is a published API | Signature churn is cheapest now, before downstream consumers exist. |

## Done

- **H1** — `charger/hardware.rs` split into `charger/hardware/{mod,charge_point,evse,connector}.rs`.
  Pure refactor; `charger/mod.rs` has a zero-line diff, test count unchanged at 115.
- **H2** — `connect_charger` drives `ChargePointBuilder` itself: dial via `ocpp_client::connect`,
  match `NegotiatedClient`, register through a `register_setup_blocks` helper that is generic over
  the CSMS client exactly as `setup()` is, then add the 2.1-only extras and seal with `build()`.
  `ChargerHardware` (in `charger/hardware_bundle.rs`) landed field-less, per decision 5.

  Three things this turned up that the plan above had wrong or didn't know:

  1. `setup()` registers **24** blocks, not the 21 written here originally — the first count missed
     `local_authorization_list`, `cost`, and `tariffs`. The vendored source is the specification;
     any summary of it, including this document, is not.
  2. The equivalence test needed two halves, not one. Comparing `ChargePointState` alone is a weak
     signal, because most registrations (`clear_cache`, `remote_control`, `trigger_message`,
     `reservation`, …) only call a `register_*` method on the CSMS client and never touch state —
     dropping one passes a pure state diff undetected. A `RecordingCsms` fake that logs every
     `register_*` call, asserted against directly, is what actually catches a dropped registration.
  3. Upstream's `WebSocketPingInterval` keepalive loop cannot be reproduced downstream at all — see
     "Known gaps" below.

- **H3** — a `SimulatedMeter` in `charger/hardware/metering.rs`, advanced by an injected `elapsed`
  and emitted per connector by `FakeChargePoint::tick`. Energy flows on `contactor_closed` — the
  hardware layer has no view of the OCPP connector state machine and doesn't need one — clamped by
  `set_current_limit`, which finally does something.

  **Scope changed during the wave.** The roadmap said to move the physics *out of*
  `EvseState::tick`. That can't happen yet: `FakeChargePoint` only exists inside `connect_charger`,
  so a local (unconnected) simulation would have no hardware to run a meter, and the TUI's
  dashboard and goldens would go with it. H3 built the hardware-side meter only; the physics are
  temporarily duplicated with `charger/state.rs`, and **H3b** below owns the convergence.

  Two things it turned up:

  1. Meter events must be addressed by **array position** (`enumerate()` over `evses`/`connectors`),
     not by `FakeConnector`'s own `evse_id`/`connector_id`, which carry the YAML's numbering — often
     1-based, possibly non-contiguous. Addressed the wrong way, every sample is silently dropped
     rather than rejected. `ocpp_bridge.rs::meter_sample_events` already had this right.
  2. The hardware meter reports `power_w`/`current_ma`/`voltage_v` as `Some(0)` when idle, where
     `ocpp_bridge.rs` leaves them `None` — "measured zero" versus "cannot measure". Both defensible;
     H3b has to pick one deliberately.
- **H4** — a `capabilities:` block in the charger YAML, `deny_unknown_fields` so a typo'd flag is a
  parse error rather than a silent `false`, reaching the hardware through
  `ChargerConfig::capabilities()`. The legacy top-level `has_display:` key still parses and still
  lands in `Capabilities`.
- **H5a** — `FileStorage`: one file per key under a caller-supplied directory. Keys are hex-encoded
  with a `k` prefix, a lossless bijection that as a side effect makes `/`, `\`, NUL, `..`, dotfiles,
  and Windows reserved device names all unrepresentable. Writes are temp file → `sync_all` →
  rename, so a reader sees the whole old value or the whole new one.
- **H6a** — `FakeDisplay`, recording what it was told to show. State is a three-way
  `Never`/`Cleared`/`Message` enum rather than an `Option`, because upstream defines `show(None)` as
  "clear the screen", not "no change". `supported_formats` deliberately omits `Html`/`Uri`/`QrCode`
  so the handler's `NotSupportedMessageFormat` path stays exercisable.
- **H3b** — the convergence: `EvseState::tick` no longer computes anything electrical (only
  `session_duration`/SoC remain, per decision — no vehicle model exists down in the hardware layer
  yet); `apply_ocpp_state` is now the single path into `ChargerState`, for a local charger and a
  live-CSMS one alike, both `connection_status` (via `SimulationMode`) and `EvseMetrics` (summed
  from `ChargePointState`'s per-connector `latest_meter_samples`). `ocpp_bridge::meter_sample_events`
  and the TUI's `maybe_send_meter_values` are gone; the TUI forwards its own tick cadence straight
  into `RunningCharger::tick` instead.

  **The handle problem.** `connect_charger` moves a `FakeChargePoint` into `ChargePointBuilder`,
  which wraps it in its own internal `Arc` nothing outside `ocpp_charge_point` can reach again — so
  the caller loses the only handle that could tick it afterwards. Fixed at the source:
  `FakeChargePoint` is now a cheap `Clone` (an `Arc`-backed newtype internally), so both
  `connect_charger` and the new `start_local_charger` clone it *before* handing one clone's
  ownership away, and bundle the surviving clone with the runtime in a new `RunningCharger`
  (`Deref`s to `ChargePointRuntime` for everything but `tick`). One type, either constructor.

  **Local mode needed an Authorizer, not just a runtime.** A bare `ChargePointRuntime::new` has no
  functional blocks at all — including Authorization — so a connector presenting an identifier
  would sit in `Authorizing` forever with nothing to answer it, and the meter (gated on the
  contactor, which only closes once a connector reaches `Charging`) would never move. `start_local_charger`
  goes through `ChargePointBuilder` after all, registering only `authorization()` against a trivial
  always-accept `LocalAuthorizer` — the same stance `ocpp-charge-point`'s own
  `examples/simulated_charge_point.rs` takes for its no-CSMS mode. No dial, no `register`/
  `register_until_accepted` call, so `ChargePointState::registration` stays `None` forever and
  `apply_ocpp_state` reads that (via `SimulationMode::Local`) as `Offline`.

  Decision made: idle meter fields read `Some(0)` (the hardware's answer), never `None` — settled
  by construction once `meter_sample_events` (the only caller of the `None` convention) was deleted.

- **H5b + H6b** — done as one task, since both are registrations into the same two files and
  splitting them would only have manufactured a conflict. `ChargerHardware` grew
  `Option<FileStorage>`/`Option<FakeDisplay>` (so `::default()` still means "neither"), the TUI
  passes a real bundle rooted at a per-charger state directory, and registration is gated on the
  capabilities H4 made declarable.

  The subtlety worth remembering: `status_notifications`, `transaction_events` and `security_events`
  each come in a plain and a `_persisted` form, and `register_setup_blocks` already registered the
  plain one unconditionally. They share a single-use broadcast subscription, so registering both
  isn't a loud failure — the second call silently no-ops. The resolution is either/or: plain when
  there's no storage (preserving equivalence with upstream `setup()`, which has no `Storage`
  parameter and so always uses the plain form), `_persisted` exactly when `has_persistent_storage`
  is declared. `security_log_persisted` is genuinely independent and registers alongside.

  One reasoned call that the source did not settle: `reservation_persistence`,
  `local_authorization_list_persistence` and `charging_profile_persistence` fire only when *both*
  `has_persistent_storage` and their own capability are declared, on the grounds that restoring
  state nothing will ever read is pointless.

- **H3b** — decision 1 is now true. A local charger runs a real `ChargePointRuntime`, so
  `apply_ocpp_state` is the only path into `ChargerState`, `EvseState::tick`'s physics are gone, and
  `meter_sample_events`/`maybe_send_meter_values` are retired. `FakeChargePoint` became a cheap
  `Clone` over an inner `Arc` so a handle survives being moved into the builder, bundled with the
  runtime as `RunningCharger`.

  Three things it decided that went beyond the brief, all defensible and all worth knowing:

  1. **A bare `ChargePointRuntime::new` isn't enough for local mode.** With no functional blocks
     registered, a connector presenting an identifier sits in `Authorizing` forever, so the
     contactor never closes and the meter can never move. `start_local_charger` goes through
     `ChargePointBuilder` and registers `authorization()` alone, against an always-accept
     `LocalAuthorizer` — the same stance upstream's own `examples/simulated_charge_point.rs` takes.
  2. **The simulated boot lifecycle is gone.** A local charger used to show `Booting` for ~1.5
     simulated seconds and then `Connected`; it now reports `Offline` permanently. See `CLAUDE.md` —
     this is a product change, not a bug fix, and the note that prompted it was stale.
  3. **Local-mode connector *status* still comes from the coarse `Command::apply_to` path**, while
     the meter is fully real. Routing local commands through the OCPP event pipeline is command
     routing rather than physics, and was correctly left out of scope. Until it lands, local mode is
     half-converged — which is worth fixing before anyone reads local-mode behavior as authoritative.

- **H7** — `ConnectorState` gained `locked`, `contactor_closed`, `current_limit_ma`, filled by a new
  `apply_hardware_state` next to `apply_ocpp_state` (they read different sources, so they stay
  separate functions) and joined on `RunningCharger::apply_state`, the only place a state snapshot
  and the hardware handle are both in scope. Plumbing only: nothing calls it yet, because the TUI's
  `App` holds channel endpoints rather than a hardware handle. Rendering these is a TUI task with its
  own golden review.
- **H9** — reservations and the local authorization list, proven from outside the crate via
  `tests/reservation_and_auth_list.rs`. Two things it could *not* prove, which matter more than the
  three it could:

  1. **The local authorization list is inert in local mode**, and that is our bug, not upstream's.
     H3b registered an always-accept `LocalAuthorizer` whose error type is `Infallible`; upstream only
     consults the list from the `Err(_)` arm of `plain_decision`, so that code is unreachable. A test
     seeds a list that explicitly rejects an identifier and shows charging starts anyway. See
     "Known gaps" — the fix is to make the local authorizer *fail*, which is also the more honest
     simulation of a charger that cannot reach a CSMS.
  2. **A capability flag gates handler registration, not the connector state machine.** A locally
     injected `ConnectorEvent::Reserved` transitions `Available -> Reserved` whether or not
     `capabilities.reservation` is declared. Defensible — without the handler a CSMS cannot issue
     `ReserveNow` at all, so the gate is at the protocol boundary — but worth knowing that injecting
     events locally bypasses it, and worth not mistaking for a capability check.
- **H12a** — `FileCertificateStore` wraps upstream's `StoredCertificates<FileStorage>`, so it is
  bounded and persistent for free. `FileKeyStore<C>` fixes the storage half and stays generic over
  `C: SoftwareCrypto`, because **upstream ships the `SoftwareCrypto` trait and no implementation** —
  correctly refusing to invent crypto rather than shipping something homegrown. Choosing a real
  backend is a security decision left to H13. Moved `chrono` from a dev- to a regular dependency,
  since `CertificateStore::expires_at` names `chrono::DateTime<Utc>` in non-test code.

- **H8** — the smart-charging chain is proven end to end, from outside the crate
  (`tests/smart_charging.rs`): a restrictive profile accrues measurably less energy, `Some(0)`
  suspends without ending the transaction or faulting, clearing restores the rate, a stepped
  schedule switches exactly at the period boundary, and an undeclared capability leaves the profile
  inert. Every link works — **once actually wired up**, which local mode does not do. See H3c.

## Where we are

Every charger — local or connected — runs a real `ocpp_charge_point::ChargePointRuntime` (see
`charger::RunningCharger`), driving the exact same connector state machine and hardware layer:

- `FakeChargePoint` / `FakeEvse` / `FakeConnector` implement `ChargePoint`, `Evse`, and
  `Connector` — the three required traits, plus a per-connector `SimulatedMeter`.
- `capabilities()` returns `Capabilities::default().with_has_display(config.has_display)`: every
  other flag is `false`, so the CSMS is told nothing the simulator can't do.
- `set_current_limit` clamps `SimulatedMeter`'s simulated current draw.
- `Evse::reboot` logs and returns `Ok`.
- `charger/state.rs` no longer simulates anything electrical or drives `connection_status` itself;
  `ocpp_bridge::apply_ocpp_state` is the only thing that writes either, from a real
  `ChargePointState` snapshot.

One simulation, as the guiding principle at the top of this document says.

## Decisions taken

These are the calls this roadmap is built on. Revisit them here rather than re-deciding per task.

1. **The fake hardware becomes the only simulation.** The physics move out of `EvseState::tick`
   into the hardware layer, which pushes `ConnectorEvent::MeterValueSampled` through
   `HardwareEventSender`; `ChargerState` becomes a projection of what comes back. Keeping both
   means connected mode and local (1.6J / unconnected) mode drift apart, and it is the reason
   `set_current_limit` currently has nowhere to apply itself.
2. **Simulated time stays injected.** The hardware exposes a `tick(elapsed)`; it does not own a
   `tokio::time::interval` internally. The TUI already drives `ChargerState::tick` this way and
   its goldens depend on it. A convenience that spawns a ticker may wrap this later; the
   deterministic entry point is the one tests and frontends use.
3. **Floats inside, integers at the boundary.** `EvseMetrics` keeps `f64` kW/A/kWh; conversion to
   `MeterSample`'s `i64` Wh/W/mA and `u8` SoC happens at the point of emission. Mixing the two
   inside the accumulator is how the SoC-progression bug in the TUI roadmap happened.
4. **Power is signed from day one.** V2G is on this roadmap; discovering that export needs a sign
   after the accumulator is written is a rework nobody needs. `MeterSample`'s fields are already
   `i64`.
5. **Optional hardware is bundled, not appended.** `connect_charger` takes a `ChargerHardware`
   struct rather than growing a parameter per trait. One breaking change to a published signature
   instead of eight.

## The blocker to clear first

`connect_charger` goes through `ocpp_charge_point::connect_and_setup`, and that function's
signature cannot receive optional hardware at all. Every trait beyond the base three — `Storage`,
`Display`, `FirmwareInstaller`, `FirmwareVerifier`, `FileTransfer`, `CertificateStore`,
`KeyStore`, `OcspChecker`, `Iso15118Controller`, `PaymentTerminal`, `BatterySwapStation`,
`Watchdog` — is registered per functional block on `ChargePointBuilder`, whose ~55 registration
methods are the only way in. `setup()` is a fixed "everything on" wrapper over the same builder;
there is no way to add a registration after it returns.

So nothing past step H3 can land until core drives the builder itself. Two things make that
cheaper than it sounds, and one makes it more expensive:

- Cargo features are not in the way. The crate's `default` feature already enables
  `smart-charging`, `display-message`, `reservation`, `local-auth-list`, `firmware-management`,
  `firmware-publishing`, `diagnostics`, `variable-monitoring`, `tariff-cost`, `payment`,
  `iso15118`, `der-control`, `battery-swap`, `periodic-event-stream`, `certificates`,
  `certificate-management`, `key-storage`, and `ocsp-checking`.
- Everything `connect_and_setup` does internally is publicly reachable: `ocpp_client::connect`
  returns the `NegotiatedClient` enum, and `network_switch::ConnectionTarget` (`new`, `install`,
  `set_version`, `set_max_inbound_frame_bytes`, `attach_security_reporting`) is a public module.
  The migration can be a faithful reproduction, not a reinvention.
- But `setup()` registers 24 blocks — 13 unconditional, and 11 gated across six `if capabilities.*`
  blocks (`reservation` + `reservation_status_updates`; `local_authorization_list`; `cost` +
  `tariffs`; `smart_charging` + `charging_profile_reports`; `variable_monitoring` +
  `monitoring_reports` + `variable_monitor_events`; `periodic_event_streams`). 2.1-only extras
  live above it in `connect_and_setup`: priority charging plus its notification worker, dynamic
  charging profiles, and network-profile switching (which also attaches security reporting to the
  redial target — one builder call does both). Reproducing that list is the actual work of H2, and
  silently dropping one of them is the failure mode to test against.

## Tasks

Each task names the files it owns, so two tasks in the same wave never edit the same file.

### H1 — Split `charger/hardware.rs` into a module directory

**Owns:** `charger/hardware.rs` → `charger/hardware/{mod,charge_point,evse,connector}.rs`
**Depends on:** nothing.

Pure refactor, no behavior change, ~30 minutes. Its entire purpose is to stop every later task
from queueing behind the same 300-line file. Do it first even though it delivers nothing on its
own. `charger/mod.rs`'s `pub use hardware::{FakeChargePoint, FakeConnector, FakeEvse}` stays
byte-identical, so nothing downstream notices.

**Done when:** `cargo test` is green and `git diff --stat` shows only moves.

### H2 — Drive `ChargePointBuilder` from `connect_charger`

**Owns:** `charger/connect.rs`, and a new `charger/hardware_bundle.rs` for `ChargerHardware`.
**Depends on:** nothing (can run alongside H1 — different files).

Replace `connect_and_setup` with: dial via `ocpp_client::connect`, match `NegotiatedClient`,
`ChargePointBuilder::start`, reproduce `setup()`'s 21 registrations plus the 2.1-only extras, then
`build()`. Introduce `ChargerHardware { storage, display, firmware, … }` defaulting every field to
the crate's `No*` null implementations, and thread it through `connect_charger`.

The registration list is the risk. Write it against `setup.rs`'s source order and add a test that
asserts the resulting `ChargePointState` has the same shape as one built by `connect_and_setup`
for an all-false `Capabilities`, so a dropped registration shows up as a diff rather than as a
CSMS message that mysteriously goes unanswered six months later.

**Done when:** `connect_charger` returns the same `ChargePointRuntime` behavior as before against
the local dev CSMS (the existing `#[ignore]`d integration test, run manually), and
`ChargerHardware::default()` is provably equivalent to today's wiring.

### H3 — Move the meter physics into the hardware

**Owns:** `charger/hardware/connector.rs`, new `charger/hardware/metering.rs`.
**Depends on:** H1.

Move `SIMULATED_CHARGING_POWER_KW`, `NOMINAL_VOLTAGE`, `SOC_PERCENT_PER_SECOND` and the
accumulation loop out of `EvseState::tick` and into a per-connector simulated meter, driven by
`tick(elapsed)` (decision 2) and emitting `MeterSample` with `power_w`, `current_ma`, `voltage_v`
and `soc_percent` populated — not just `energy_wh` as today.

This is where `set_current_limit` finally means something: the limit clamps simulated current,
which sets power, which sets the energy rate.

**Done when:** `Some(0)` halts energy accumulation while leaving the transaction alive, `None`
restores the unlimited rate, `Some(8_000)` yields roughly half the energy of `Some(16_000)` over
the same elapsed time, and results are identical whether one large `elapsed` or 100 small ones are
fed in (the cadence trap from the TUI roadmap's working agreements).

### H4 — `Capabilities` from YAML config

**Owns:** `charger/config.rs`, `charger/hardware/charge_point.rs`.
**Depends on:** H1.

A `capabilities:` block in the charger YAML mapping onto `Capabilities`'s builder methods
(`with_smart_charging`, `with_reservation`, …), defaulting to all-false. `has_display` moves from
its ad-hoc top-level field into the block, keeping the old key parsing for compatibility with the
example configs.

Flags only go `true` when hardware backs them, so this task ships the plumbing and the existing
`has_display`; later tasks each flip their own flag. Upstream's `warn_on_feature_mismatches`
catches drift between what's declared and what's registered — call it and route it into `tracing`
so it lands in the TUI log pane.

**Done when:** a YAML fixture with three capabilities set round-trips into the right
`Capabilities`, and an unknown capability key is a parse error rather than a silent ignore.

### H5 — `Storage`: a file-backed implementation

**Owns:** new `charger/hardware/storage.rs`.
**Depends on:** H1 for placement (H5a); H2 for wiring (H5b).

Split deliberately, because the halves parallelize:

- **H5a** — implement `Storage` (`get`/`set`/`remove`, all `async`, `Vec<u8>` values) over a
  per-charger directory under the existing config dir. Self-contained, unit-testable with
  `tempfile`, needs nothing from H2. Upstream's `InMemoryStorage` (behind `std`) is the reference
  for semantics.
- **H5b** — register it: `boot_reason_persistence`, `transaction_persistence`,
  `authorization_cache_persistence`, `local_authorization_list_persistence`,
  `device_model_persistence`, `network_profile_persistence`, `status_notifications_persisted`,
  `transaction_events_persisted`, `security_events_persisted`.

Highest value per line on this roadmap: it makes a simulated charger survive a restart, which is
what makes offline-queue and boot-reason behavior testable at all.

**Done when:** a charger that stops mid-transaction and reconnects reports the same transaction,
and the offline queue flushes what it buffered while disconnected.

### H6 — `Display`: a recording fake

**Owns:** new `charger/hardware/display.rs`.
**Depends on:** H1 (H6a); H2 (H6b, `display_messages` registration).

`show(Option<&DisplayedMessage>)` stores the current message; `supported_formats()` returns a
deliberately restricted list — `MessageFormat::Ascii` and `Utf8`, but not `Html`, `Uri`, or
`QrCode` — so the handler's `NotSupportedMessageFormat` rejection path is exercised rather than
assumed. Same a/b split as H5.

Pairs with a TUI panel rendering the charger's screen — tracked in the TUI roadmap, not here.

### H3b — Converge the two simulations

**Owns:** `charger/state.rs`, `charger/ocpp_bridge.rs`, and the TUI's tick plumbing.
**Depends on:** H3.

The half of H3 that got deferred, and the task that finally makes decision 1 true. `ChargerState`
stops simulating and starts projecting: delete the physics from `EvseState::tick`, drive
`FakeChargePoint::tick` instead, and retire `ocpp_bridge.rs::meter_sample_events` along with the
TUI's `maybe_send_meter_values`.

The hard part is not the connected path — it's that a local (1.6J / unconnected) simulation has no
`FakeChargePoint` at all today, because one is only built inside `connect_charger`. Something has to
own fake hardware for an unconnected charger before the physics can move. Settle that first; the
rest is mechanical.

Also decide, deliberately: idle meter fields read `Some(0)` (hardware) or `None` (bridge)?

Expect TUI goldens to move. Per the TUI roadmap's working agreements, inspect every regenerated one
rather than accepting the diff.

### H3c — Register the same functional blocks in local mode

**Owns:** `charger/running_charger.rs`, `charger/connect.rs`.
**Depends on:** nothing outstanding. **Do this before wave 4** — it invalidates conclusions drawn
from local-mode behavior until it lands.

`start_local_charger` registers exactly one block: `authorization`, against the always-accept
`LocalAuthorizer`. Nothing else. Smart charging, reservations, the local authorization list,
meter values, status notifications, the device model — all absent. A local charger is a real state
machine with real hardware and almost no functional blocks attached, so a CSMS-independent feature
does not fail loudly; it silently does nothing.

That is how H8 found its gap (an injected `ChargingProfileSet` lands in state and no limit is ever
computed, because the projection loops `ChargePointBuilder::smart_charging` spawns were never
started) and it is half of why H9 could not observe local-list rejection. Both had to build their
own builder chain in a test to work around it. A downstream consumer reaching for
`start_local_charger` sees the same silent no-op, which is worse for them than for us — they have no
roadmap explaining it.

The fix is to route local mode through the same `register_setup_blocks` the connected path uses,
against a null CSMS that satisfies the handler-registration traits without sending anything. H8
demonstrated in `tests/smart_charging.rs` that every type required is public, so no upstream change
is needed. Take the local-authorizer fix in "Known gaps" at the same time: both are "local mode is
under-wired", and testing them together is cheaper than twice.

Do not let the null CSMS quietly *become* a CSMS. It must not fabricate authorization decisions,
transaction acknowledgements, or boot responses; anything a real CSMS would answer, it should
decline to answer, so that local mode exercises the offline paths rather than a fake-online one.

### H7 — Surface hardware state on `ChargerState`

**Owns:** `charger/state.rs`, `charger/ocpp_bridge.rs`.
**Depends on:** H3 (for the current limit to be worth showing).

Lock, contactor, and applied current limit are tracked by `FakeConnector` but invisible to any
frontend — the "lock and contactor state are unreachable" gap in the TUI roadmap. Project them
onto `ConnectorState` through the existing `apply_ocpp_state` path.

### H8 — Smart charging, end to end

**Owns:** `charger/hardware_bundle.rs` (registration), `charger/config.rs` (flag).
**Depends on:** H2, H3, H4.

Flip `smart_charging`, register `smart_charging`, `charging_profile_persistence`,
`charging_profile_reports`, `dynamic_charging_profiles`, `priority_charging`. The composite
schedule then drives `set_current_limit`, which H3 made real. This is the load-balancing scenario
named in `CLAUDE.md`.

**Done when:** a CSMS-installed profile with a stepped schedule visibly changes simulated power at
each period boundary, and clearing the profile restores the unlimited rate.

### H9 — Reservation and local authorization list

**Depends on:** H2, H4, H5b. **Parallel with:** H8, H10.

Mostly state rather than hardware: flip the two capability flags and register `reservation`,
`reservation_status_updates`, `local_authorization_list`, plus their persistence. The
`ConnectorState::Reserved` → `ConnectorStatus::Reserved` mapping already exists.

### H10 — Firmware and diagnostics

**Owns:** new `charger/hardware/firmware.rs`, `charger/hardware/file_transfer.rs`.
**Depends on:** H2 for wiring; the impls are independent. **Parallel with:** H8, H9.

`FirmwareInstaller` / `FirmwareVerifier` / `FileTransfer` fakes that report staged progress over
simulated time (decision 2 again — no `sleep`), so a CSMS firmware campaign and a log upload can
both be driven to completion, and to failure. Register via `firmware_updates`, `log_uploads`,
`publish_firmware`.

### H11 — `Watchdog` — **blocked upstream, do not schedule**

Not implementable from a downstream crate, for the same reason the keepalive loop isn't. There is no
`ChargePointBuilder::watchdog` method, `ChargePointRuntime::new` takes no watchdog, and the only
public entry point — `ChargePointActor::spawn_with_watchdog` — returns an actor that cannot be handed
to a runtime or a builder, since `ChargePointRuntime::actor()` is `pub(crate)`. A custom `Watchdog`
therefore cannot take part in a real session; every session gets upstream's `NoWatchdog`.

Fold this into the same upstream conversation as the keepalive gap under "Known gaps" — both are
symptoms of the actor being private, and one upstream change (a public `actor()`, or builder methods
that accept these) would unblock both.

### H12 — Certificates and key storage

**Owns:** new `charger/hardware/certificates.rs`, `charger/hardware/keys.rs`.
**Depends on:** H5b (persistence). **Parallel with:** H10.

`CertificateStore` and `KeyStore` over H5's storage — upstream ships `SoftKeyStore` and
`SoftwareCrypto`, so this is mostly wiring plus a persistence backing. Register `certificates`,
`ocsp_status`, `ocsp_chain_status`.

### H13 — ISO 15118 and plug and charge

**Depends on:** H12.

`Iso15118Controller` plus `with_iso15118_support`, and the vehicle model gaining a contract
certificate. The largest task here, and the one most likely to want its own sub-plan once H12
lands.

### H14 — V2G and DER control

**Depends on:** H3 (signed power), H4.

`with_supports_bidirectional_power`, `with_der_control`, `der_control` registration, and a
discharge mode on the simulated meter. Cheap *if* decision 4 held; expensive if it didn't.

## Parallelism

No two tasks in the same wave touch the same file.

If a wave is run by agents in git worktrees, check the base commit before anything else: a
worktree may be created from the default branch rather than from the branch the previous wave
landed on. H1's first attempt refactored long-superseded code that way, passed its own tests
against it, and had to be thrown away. `git reset --hard <branch>` plus an assertion about
something only the current branch has is the cheap guard.

| Wave | Tasks | Notes |
| --- | --- | --- |
| 0 | ~~**H1**, **H2**~~ | Done. Different files, so both at once. H1 was hours; H2 was the long pole, as expected. |
| 1 | ~~**H3**, **H4**, **H5a**, **H6a**~~ | Done, four-way parallel. H5a and H6a were the cheapest to hand off, exactly as predicted — pure trait impls, no dependency on H2. |
| 2 | ~~**H5b+H6b**~~, ~~**H3b**~~, **H7** | H5b+H6b done as one task. H3b followed, and ended up owning `charger/connect.rs` too for the handle problem — see its "Done" entry. H7 is what's left. H11 turned out to be blocked upstream; see its section. |
| 3 | **H8**, **H9**, **H10**, **H12** | The widest wave: four independent functional blocks. H8 and H9 share `hardware_bundle.rs`, so sequence those two or split the file by block first. |
| 4 | **H13**, **H14** | H13 needs H12; H14 needs only H3, so H14 can be pulled into wave 3 if someone is free. |

The critical path is **H2 → H5b → H12 → H13**. Everything else has slack. If H2 slips, waves 2–4
all slip with it, which is the argument for starting it before H1 despite H1 being the smaller,
more satisfying task.

## Working agreements

- Test-first, per `CLAUDE.md`: every task above names its "done when" as an assertion, not a
  feeling.
- A capability flag and the hardware behind it land in the same commit. A `true` with nothing
  behind it is worse than a `false` — it makes the CSMS ask questions the simulator can't answer.
- Never `sleep` in simulated behavior. Progress is a function of injected `elapsed`, so a firmware
  install that "takes 90 seconds" completes instantly in tests and paces itself in the app.
- Return `Err` from a fake rather than panicking. Upstream turns a hardware error into a fault
  state, which is exactly the behavior worth being able to trigger on demand.
- Watch the boundary conversions. `MeterSample` is `i64` Wh/W/mA and `u8` percent; the accumulator
  is `f64`. Round at emission, never in the accumulator.
- `core` is published. Any change to `connect_charger`, `ChargerConfig`, or the `Fake*` types is a
  change to somebody else's build.
- Adding a field to `ChargerConfig` breaks every exhaustive literal in the workspace — currently 15
  of them. That is deliberate: the struct is not `#[non_exhaustive]`, and a compile error at each
  site is the right prompt to think about what the new field should be there. Don't "fix" it with
  `..Default::default()`, which would silently absorb the next field too. Do budget for it when
  scoping a task, and don't hand the field addition and the call sites to different agents.
- File ownership only parallelizes tasks that are genuinely separable in Rust. H4 owned `config.rs`
  alone, but its one new field made four other files stop compiling — so its commit could not stand
  on its own, and the integration landed here instead. When a task changes a widely-constructed
  type, it owns the ripple too.

## Known gaps

- **The local authorization list can never reject anything (ours to fix, and worth doing soon).**
  H3b's `LocalAuthorizer` returns `Ok(Accepted)` for everything and is `Infallible`, so upstream's
  `offline_decision` — the only code that reads `local_authorization_list.entries` — is unreachable.
  The fix is to give the local authorizer a real error type and return `Err`, which is what a charger
  with no CSMS *actually* experiences: the request cannot reach anyone, so the crate falls back to
  the local list and the auth cache. That makes local mode both more honest and more useful, since
  offline authorization is one of the more valuable things to be able to demonstrate.

  Deliberately not fixed during wave 3: H8 was mid-flight driving local chargers to `Charging`
  through `IdTokenPresented`, and changing authorization semantics underneath it would have broken
  its branch on merge. Do it as its own task, and expect to seed a local list in any test that
  currently relies on everything being accepted.
- **No positive-case test for capability-gated registration.** `connect.rs` proves only the negative
  (all-false capabilities registers no gated block). Nothing asserts that declaring `reservation` or
  `local_auth_list` actually *does* register `reserve_now`/`send_local_list`. H9 was scoped out of
  `connect.rs` and could not add it from an integration test.
- **No `SoftwareCrypto` backend ships.** `FileKeyStore` stays generic until someone picks one; H13
  cannot do plug and charge without that decision. `ring` is already present transitively via the
  websocket TLS stack, which makes it the obvious candidate — but it is a security choice, not a
  convenience one.

- **The keepalive ping loop is gone, and cannot be brought back from here.** Upstream's
  `connect_and_setup` spawns `keepalive::run_ping_interval_updates`, which applies a CSMS-written
  `WebSocketPingInterval` device-model variable to the live connection. It needs a
  `ChargePointActor`, obtainable only via `ChargePointRuntime::actor()` — which is `pub(crate)` and
  exposed nowhere on `ChargePointBuilder`. So this is the one thing the builder path genuinely
  cannot match, and the price paid for being able to register optional hardware at all. Fixing it
  means an upstream change: either make `actor()` public or add a builder method that spawns the
  loop. H5b has since made the builder path permanent, so this is now a live gap rather than a
  theoretical one.
- **A custom `Watchdog` cannot be installed at all** — same root cause as the keepalive gap, see H11.
  One upstream change (a public `actor()`, or builder methods accepting these) would unblock both,
  which is the shape the request to `ocpp-charge-point` should take.
- **`FileStorage` doesn't bound encoded key length.** Hex doubles it, so a key over ~127 bytes would
  exceed the 255-byte filename limit and surface as an opaque `ENAMETOOLONG`. Latent, not live:
  every key upstream currently uses is a short constant (`ocpp-cp/auth-cache`, `ocpp-cp/txn`, …) or
  built from small integers, the longest around 25 characters. Worth a guard returning a real error
  before anything starts deriving keys from CSMS-supplied data.
- **`connect_charger` is OCPP 2.1 only, now explicitly.** A CSMS that negotiates 1.6J or 2.0.1 gets
  `ConnectAndSetupError::UnsupportedNegotiatedVersion` instead of a session. That matches how the
  TUI already gates the call and the function's long-standing doc comment, but it is a narrowing:
  `connect_and_setup` would have run those versions. Driving their builder chains is its own task,
  worth scheduling once someone actually wants 1.6J against a live CSMS.

## Not on this roadmap

- **Fault injection.** Upstream's `hardware::fault_injection` is `#[cfg(test)]`-only, so a
  simulator-side equivalent would be ours to build. Worth doing — a "stick the contactor" command
  is exactly what this tool is for — but it's a feature, not part of extending the trait surface.
- **`PaymentTerminal` / `BatterySwapStation`.** Both feature-gated upstream and both niche for a
  CSMS-development simulator. Pick them up if someone asks.
- **OCPP message counters.** Still blocked on upstream exposing sent/received counts; see the TUI
  roadmap's known gaps.
