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

## Where we are

The baseline, after moving from the `ocpp-charge-point` git dependency to the published 0.1.0:

- `FakeChargePoint` / `FakeEvse` / `FakeConnector` implement `ChargePoint`, `Evse`, and
  `Connector` — the three required traits, and nothing else.
- `capabilities()` returns `Capabilities::default().with_has_display(config.has_display)`: every
  other flag is `false`, so the CSMS is told nothing the simulator can't do.
- `set_current_limit` records its argument and is otherwise inert. Nothing consumes it.
- `Evse::reboot` logs and returns `Ok`.
- The charging physics live somewhere else entirely: `EvseState::tick` in `charger/state.rs`
  accumulates energy, power, current, session duration, and SoC, and the TUI pushes the result
  into the OCPP stack as `MeterSample { energy_wh, .. }` via `meter_sample_events`. The hardware
  layer contributes nothing to it.

So there are two simulations that don't know about each other, and the hardware one is empty.

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
- But `setup()` registers 21 blocks (13 unconditional, 8 capability-gated), and 2.1-only extras
  live above it in `connect_and_setup`: security reporting attached to the redial target,
  priority charging plus its notification worker, and network-profile switching. Reproducing that
  list is the actual work of H2, and silently dropping one of them is the failure mode to test
  against.

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

### H11 — `Watchdog`

**Depends on:** H2. Trivial; slot it into any wave that has capacity.

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

| Wave | Tasks | Notes |
| --- | --- | --- |
| 0 | **H1**, **H2** | Different files, so both at once. H1 is hours; H2 is the long pole of the whole roadmap — start it first if only one person is on this. |
| 1 | **H3**, **H4**, **H5a**, **H6a** | Four-way parallel. H5a and H6a are pure trait impls with `tempfile`-backed tests and no dependency on H2 at all — the cheapest work to hand to a second pair of hands. |
| 2 | **H5b**, **H6b**, **H7**, **H11** | All small; H5b/H6b are registrations that H2 made possible, H7 is frontend plumbing. |
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

## Not on this roadmap

- **Fault injection.** Upstream's `hardware::fault_injection` is `#[cfg(test)]`-only, so a
  simulator-side equivalent would be ours to build. Worth doing — a "stick the contactor" command
  is exactly what this tool is for — but it's a feature, not part of extending the trait surface.
- **`PaymentTerminal` / `BatterySwapStation`.** Both feature-gated upstream and both niche for a
  CSMS-development simulator. Pick them up if someone asks.
- **OCPP message counters.** Still blocked on upstream exposing sent/received counts; see the TUI
  roadmap's known gaps.
