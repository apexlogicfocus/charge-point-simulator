//! The shared result of actually running a charger's fake hardware against a real
//! `ocpp_charge_point` state machine - see [`RunningCharger`] and [`start_local_charger`], and
//! `docs/hardware-roadmap.md`'s H3b for why this module exists.

use std::ops::Deref;
use std::sync::Arc;
use std::time::Duration;

use ocpp_charge_point::ChargePointBuilder;
use ocpp_charge_point::ChargePointRuntime;
use ocpp_charge_point::clock::{SystemClock, SystemMonotonicClock};
use ocpp_charge_point::executor::TokioExecutor;
use ocpp_charge_point::hardware::{ChargePoint, Evse};
use ocpp_charge_point::provisioning::TokioBackoff;

use super::config::ChargerConfig;
use super::connect::{NullCsms, register_optional_hardware, register_setup_blocks};
use super::hardware::{FakeChargePoint, FakeConnector, FakeFileTransfer, FakeFirmwareInstaller};
use super::hardware_bundle::ChargerHardware;
use super::ocpp_bridge::{
    ConnectorHardwareSnapshot, apply_hardware_state, apply_ocpp_state, hardware_snapshot,
};
use super::state::ChargerState;

/// Returned by [`RunningCharger::set_discharging`]/[`RunningCharger::exported_energy_wh`] when
/// `evse_id`/`connector_id` doesn't name a connector this charger actually has - addressed
/// positionally, exactly like [`FakeChargePoint::tick`]/[`apply_hardware_state`] (see either's own
/// doc comment for why array position, not the YAML config's own EVSE/connector numbers).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct NoSuchConnector {
    pub evse_id: usize,
    pub connector_id: usize,
}

impl std::fmt::Display for NoSuchConnector {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(
            f,
            "no connector at evse {} connector {}",
            self.evse_id, self.connector_id
        )
    }
}

impl std::error::Error for NoSuchConnector {}

/// A charge point whose fake hardware is actually running against a real
/// `ocpp_charge_point::ChargePointRuntime` - the shared result of both
/// [`super::connect::connect_charger`] (dialed against a real CSMS, registered, every functional
/// block wired up) and [`start_local_charger`] (no CSMS, no registration, the same state machine
/// and hardware running with nothing on the other end).
///
/// # Why this exists (`docs/hardware-roadmap.md` H3b)
///
/// `ChargePointRuntime::new`/`ChargePointBuilder::start` both take their hardware binding `T` by
/// value and wrap it in their own internal `Arc<T>` that nothing outside `ocpp_charge_point` can
/// ever reach again (`ChargePointRuntime::hardware_handle` is `pub(crate)` to that crate). So the
/// instant a `FakeChargePoint` is handed to either one, its caller loses the only handle that
/// could call [`FakeChargePoint::tick`] afterwards - exactly the problem for a charger whose
/// meter needs to keep advancing for as long as it runs.
///
/// The fix is `FakeChargePoint` itself: it is cheaply [`Clone`] (see its own doc comment), backed
/// by a shared `Arc` internally, so a caller here clones it *before* handing one clone's
/// ownership away and keeps the other as a live handle. `RunningCharger` bundles that handle with
/// the runtime so callers - the TUI, today; any other frontend later - get exactly one type to
/// hold onto regardless of which constructor built it. That is also what makes local and
/// connected chargers driven by the same code from here on: everything downstream of this struct
/// (ticking the meter, reading `state()`, dispatching a `ChargePointEvent`) is identical for both.
///
/// Every [`ChargePointRuntime`] method beyond ticking (`state`, `subscribe`, `send`, ...) is
/// reached through [`Deref`] rather than re-exposed one by one, so this stays a thin wrapper that
/// tracks upstream's own surface instead of drifting from it.
///
/// # What `tick` drives (`docs/hardware-roadmap.md` H3d)
///
/// `RunningCharger` is the one place both the ticking caller (the TUI's `drive_running_charger`,
/// today) and every piece of the fake hardware bundle that needs simulated time are in scope
/// together - the same reasoning [`Self::apply_state`] already relies on for `hardware`. Before
/// H3d, `tick` advanced only [`FakeChargePoint`] (the meter), so a CSMS-initiated firmware
/// install or file transfer registered through [`super::connect::register_optional_hardware`] sat
/// at 0% forever in the running app; the only place it ever progressed was a test holding its own
/// `Arc` and ticking the installer directly. `firmware_installer`/`file_transfer` are held here for
/// exactly the same "clone before handing ownership away" reason [`FakeChargePoint`] itself is
/// (see [`super::connect::connect_charger`]/[`start_local_charger`]'s own doc comments): both
/// callers clone their `Arc`s before consuming the originals via `register_optional_hardware`, and
/// hand the clones to [`Self::new`] here. A caller of [`Self::tick`] therefore never needs to know
/// which pieces of the bundle exist or need ticking separately - it drives the whole simulation
/// with one call, exactly like ticking a real charger's clock would.
pub struct RunningCharger {
    runtime: ChargePointRuntime<FakeChargePoint>,
    hardware: FakeChargePoint,
    firmware_installer: Option<Arc<FakeFirmwareInstaller>>,
    file_transfer: Option<Arc<FakeFileTransfer>>,
}

impl RunningCharger {
    pub(crate) fn new(
        runtime: ChargePointRuntime<FakeChargePoint>,
        hardware: FakeChargePoint,
        firmware_installer: Option<Arc<FakeFirmwareInstaller>>,
        file_transfer: Option<Arc<FakeFileTransfer>>,
    ) -> Self {
        Self {
            runtime,
            hardware,
            firmware_installer,
            file_transfer,
        }
    }

    /// Advances the whole simulation by `elapsed`: the meter ([`FakeChargePoint::tick`]), and -
    /// H3d - any in-flight firmware install or file transfer the bundle carries
    /// ([`FakeFirmwareInstaller::tick`]/[`FakeFileTransfer::tick`], both no-ops when nothing is in
    /// flight, so calling this on a charger with neither registered costs nothing). The only entry
    /// point through which this charger's simulated time ever moves, for a local charger and a
    /// live-CSMS one alike (decision 2 in `docs/hardware-roadmap.md`: simulated time only,
    /// injected by the caller, never a `tokio::time::interval` owned in here) - see this type's own
    /// doc comment for why a caller never has to tick each piece separately.
    pub async fn tick(&self, elapsed: Duration) {
        self.hardware.tick(elapsed).await;
        if let Some(installer) = &self.firmware_installer {
            installer.tick(elapsed);
        }
        if let Some(transfer) = &self.file_transfer {
            transfer.tick(elapsed);
        }
    }

    /// Projects this charger's full observable state onto `charger` in one call:
    /// [`apply_ocpp_state`] against a fresh [`ChargePointRuntime::state`] snapshot, then
    /// [`apply_hardware_state`] against [`Self::hardware`] (`docs/hardware-roadmap.md`'s H7).
    ///
    /// The two stay separate functions - `apply_ocpp_state` reads a `ChargePointState` snapshot,
    /// `apply_hardware_state` reads lock/contactor/current-limit fields that live only on
    /// [`FakeConnector`] and have no OCPP counterpart at all - so each keeps a single, legible
    /// source of truth and either can be tested (or called) on its own. This method exists only
    /// because `RunningCharger` is the one place both a `ChargePointState` snapshot and the
    /// hardware handle are reachable together: `hardware` is a private field, reached by nothing
    /// outside this type but [`Self::tick`], so no caller could otherwise drive
    /// `apply_hardware_state` at all.
    ///
    /// [`FakeConnector`]: super::hardware::FakeConnector
    pub fn apply_state(&self, charger: &mut ChargerState) {
        apply_ocpp_state(charger, &self.state());
        apply_hardware_state(charger, &self.hardware);
    }

    /// This charger's hardware-only per-connector state as an owned, `Send` value - see
    /// [`ConnectorHardwareSnapshot`], and [`Self::apply_state`] for the same projection applied
    /// directly.
    ///
    /// `apply_state` covers a caller that renders on the thread it runs this charger on. A frontend
    /// that doesn't - the TUI drives the runtime on a dedicated thread, because
    /// [`super::connect::connect_charger`]'s future isn't `Send`, and any downstream consumer of
    /// the published crate with a request/response boundary is in the same position - cannot call
    /// `apply_state` at all, and cannot reach the `hardware` field either, since it is private and
    /// deliberately so. This is what such a caller forwards instead, applying it with
    /// [`apply_hardware_snapshot`] wherever its own [`ChargerState`] lives.
    ///
    /// [`apply_hardware_snapshot`]: super::ocpp_bridge::apply_hardware_snapshot
    pub fn hardware_snapshot(&self) -> Vec<Vec<ConnectorHardwareSnapshot>> {
        hardware_snapshot(&self.hardware)
    }

    /// Puts the connector at `evse_id`/`connector_id` into discharge (V2G export) or back to
    /// import (`docs/hardware-roadmap.md`'s H14b) - addressed positionally, the same way
    /// [`Self::apply_state`]/[`FakeChargePoint::tick`] address a connector.
    ///
    /// **This is the only way to trigger discharge, and that is deliberate, not an oversight.**
    /// `HardwareCommand` has exactly six variants (`LockConnector`/`UnlockConnector`/
    /// `CloseContactor`/`OpenContactor`/`Reboot`/`SetCurrentLimit`) and none of them can express
    /// direction, so no CSMS message reaching this charger can ever flip a connector into
    /// discharge - registering `ChargePointBuilder::der_control`
    /// ([`super::connect::register_der_control`]) makes this charger answer DER Control messages
    /// honestly, but stores and reports them without projecting anything onto hardware, per
    /// upstream's own "store-and-report, not actuate" scope for that block. Inventing a fake OCPP
    /// path here - e.g. quietly flipping direction from a stored `DERControl` - would claim a
    /// protocol capability this simulator does not have; see `docs/hardware-roadmap.md`'s "Known
    /// gaps" for the fuller account of why upstream blocks this. What *is* real: the metering
    /// itself, once direction is set - see [`Self::exported_energy_wh`] and H14a's own entry.
    ///
    /// This is the honest alternative: a direct, programmatic entry point for a frontend, a test,
    /// or any other downstream consumer that wants to demonstrate bidirectional metering on
    /// purpose, mirroring [`super::hardware::FakeConnector::set_discharging`] (which this calls
    /// directly) the same way [`Self::tick`] mirrors [`FakeChargePoint::tick`].
    ///
    /// Returns `Err(NoSuchConnector)` rather than panicking when `evse_id`/`connector_id` is out
    /// of range, per this crate's "return `Err` rather than panic" working agreement.
    pub fn set_discharging(
        &self,
        evse_id: usize,
        connector_id: usize,
        discharging: bool,
    ) -> Result<(), NoSuchConnector> {
        self.connector(evse_id, connector_id)?
            .set_discharging(discharging);
        Ok(())
    }

    /// Cumulative energy the connector at `evse_id`/`connector_id` has exported so far, in Wh -
    /// see [`super::hardware::FakeConnector::exported_energy_wh`]. The observation counterpart of
    /// [`Self::set_discharging`]: `MeterSample`/`ChargePointState` never carry this figure
    /// (H14a's own decision - `energy_wh` is OCPP's *import* register and must not run
    /// backwards), so reading it back through `RunningCharger`'s own surface, addressed the same
    /// positional way as every other method here, is the only way to observe it.
    pub fn exported_energy_wh(
        &self,
        evse_id: usize,
        connector_id: usize,
    ) -> Result<i64, NoSuchConnector> {
        Ok(self.connector(evse_id, connector_id)?.exported_energy_wh())
    }

    fn connector(
        &self,
        evse_id: usize,
        connector_id: usize,
    ) -> Result<&FakeConnector, NoSuchConnector> {
        self.hardware
            .evses()
            .get(evse_id)
            .and_then(|evse| evse.connectors().get(connector_id))
            .ok_or(NoSuchConnector {
                evse_id,
                connector_id,
            })
    }
}

impl Deref for RunningCharger {
    type Target = ChargePointRuntime<FakeChargePoint>;

    fn deref(&self) -> &Self::Target {
        &self.runtime
    }
}

/// Starts a charger's fake hardware with no CSMS at all: no dial, no `register`/
/// `register_until_accepted` call - `ChargePointState::registration` stays `None` forever, and
/// [`super::ocpp_bridge::apply_ocpp_state`] reads that (via `SimulationMode::Local`) as
/// permanently `ConnectionStatus::Offline` rather than the endlessly-retried "booting" a real
/// CSMS's rejected/pending registration would mean.
///
/// Otherwise this runs the same machinery the connected path does: the same [`FakeChargePoint`],
/// the same `ChargePointBuilder`/`ChargePointRuntime`, the same connector state machine, and - as
/// of `docs/hardware-roadmap.md`'s H3c - the same [`register_setup_blocks`] the connected path
/// uses, against [`NullCsms`], a CSMS stand-in that answers nothing a real CSMS would answer (see
/// its own doc comment) so this exercises the *offline* paths rather than a fake-online one.
///
/// # `hardware` (`docs/hardware-roadmap.md` H3d)
///
/// Takes a [`ChargerHardware`] exactly like [`super::connect::connect_charger`] does, and runs it
/// through the *same* registration sequence - [`register_setup_blocks`] then
/// [`register_optional_hardware`] - with `has_csms: false` the only thing that differs. Before
/// H3d, this function took no hardware at all and hardcoded `None` for storage and display, and
/// never called `register_optional_hardware`, so a local charger got no persistence, no display,
/// no firmware installer, no certificate store regardless of what a caller might have wanted to
/// give it - the same "local mode is under-wired" gap H3c had already fixed one layer up, for
/// functional blocks rather than hardware.
///
/// `firmware_installer`/`file_transfer` are cloned before `hardware` is consumed by
/// `register_optional_hardware`, so [`RunningCharger`] still has a live handle to tick even though
/// `register_optional_hardware` itself only ever registers `certificates` locally (`firmware_updates`/
/// `log_uploads` are has_csms-gated - see that function's own doc comment) - installing firmware is
/// local hardware behavior independent of whether a CSMS campaign is registered to drive it (see
/// [`RunningCharger::tick`]'s doc comment for what this makes possible).
///
/// `has_csms: false` skips every block whose entire purpose is talking to a CSMS that does not
/// exist here - `provisioning` above all, since against a `NullCsms` that never fabricates
/// acceptance its retry-until-accepted call would otherwise hang this function forever. What's
/// left registered: `authorization` (via `NullCsms`, which now *declines* - see its doc comment -
/// so an identifier falls through to the local authorization list and the authorization cache
/// instead of what used to be an always-accepting, `Infallible` `LocalAuthorizer`, the fix
/// `docs/hardware-roadmap.md`'s "Known gaps" names), the handful of single-call CSMS-inbound
/// handler registrations cheap enough to keep regardless (`clear_cache`, `network_profiles`,
/// `remote_control`, `trigger_message`, `availability_control`, `reset`, `device_model`), and -
/// gated on the same `Capabilities` flags the connected path reads - `reservation`
/// (+`reservation_status_updates`, so a reservation actually expires locally),
/// `local_authorization_list`, `smart_charging` (+`charging_profile_reports`, so a locally
/// installed `ChargingProfileSet` actually computes and applies a current limit - H8's gap), and
/// persistence/display (H5b/H6b's own gating, unaffected by `has_csms`). [`register_setup_blocks`]'s
/// own `has_csms` doc comment has the full registered/skipped list and the reasoning behind each
/// entry; [`register_optional_hardware`]'s own doc comment covers `certificates`/`firmware_updates`/
/// `log_uploads`.
///
/// See `docs/hardware-roadmap.md`'s H3b for why this is the shape a local (unconnected)
/// simulation takes at all, instead of the coarse, hardware-free state machine
/// `charger/state.rs`/`charger/command.rs` used to run entirely on their own.
pub async fn start_local_charger(
    config: &ChargerConfig,
    hardware: ChargerHardware,
) -> RunningCharger {
    let charge_point = FakeChargePoint::from_config(config);
    let handle = charge_point.clone();

    let ChargerHardware {
        storage,
        display,
        firmware_installer,
        firmware_verifier,
        file_transfer,
        certificate_store,
        // `key_store`: never consumed here - see `ChargerHardware`'s own doc comment for why, and
        // how a caller reaches it instead (clone the `Arc` out before calling this function).
        ..
    } = hardware;
    // Cloned before being consumed by `register_optional_hardware` below - see this function's
    // own doc comment and `RunningCharger::tick`'s for why a live handle has to survive that call.
    let firmware_installer_handle = firmware_installer.clone();
    let file_transfer_handle = file_transfer.clone();

    let builder = ChargePointBuilder::start(charge_point, TokioExecutor)
        .await
        .unwrap_or_else(|error: core::convert::Infallible| match error {});
    let (mut builder, security_log) = register_setup_blocks(
        builder,
        &NullCsms,
        TokioBackoff,
        SystemMonotonicClock,
        SystemClock,
        storage.as_ref(),
        display,
        false, // has_csms: no CSMS is ever dialed in local mode.
    )
    .await;

    builder = register_optional_hardware(
        builder,
        &NullCsms,
        TokioBackoff,
        SystemClock,
        firmware_installer,
        firmware_verifier,
        file_transfer,
        certificate_store,
        security_log, // decision 8: share whatever `register_setup_blocks` restored, if anything.
        false, // has_csms: no CSMS is ever dialed in local mode - see the function's own doc comment.
    )
    .await;

    RunningCharger::new(
        builder.build(),
        handle,
        firmware_installer_handle,
        file_transfer_handle,
    )
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::charger::config::CapabilitiesConfig;
    use crate::charger::config::EvseConfig;
    use crate::charger::config::OcppVersion;
    use crate::charger::ocpp_bridge::apply_hardware_snapshot;
    use ocpp_charge_point::hardware::FirmwareInstaller;
    use ocpp_charge_point::persistence::{SecurityLogStore, restore_security_log};
    use ocpp_charge_point::security::SecurityEventLog;
    use ocpp_charge_point::state::{
        AuthorizationStatus, ChargePointEvent, ConnectorEvent,
        ConnectorState as OcppConnectorState, EvseEvent, IdToken, IdTokenKind, LocalListEntry,
        SecurityEvent, SecurityEventType,
    };
    use std::time::Duration as StdDuration;

    use super::super::hardware::FileStorage;

    fn config(evses: Vec<EvseConfig>) -> ChargerConfig {
        ChargerConfig {
            id: "CP001".into(),
            ocpp_version: OcppVersion::V21,
            evses,
            has_display: false,
            capabilities: Default::default(),
        }
    }

    fn connector_event(
        evse_id: usize,
        connector_id: usize,
        event: ConnectorEvent,
    ) -> ChargePointEvent {
        ChargePointEvent::Evse {
            evse_id,
            event: EvseEvent::Connector {
                connector_id,
                event,
            },
        }
    }

    /// Drives `charger` through a full local session up to (but not including) ticking the
    /// meter: cable connected, locked (automatic, via the real hardware round trip), an
    /// identifier presented, and authorized - the same sequence a real offline charger runs, with
    /// no CSMS anywhere in the loop.
    ///
    /// Authorization now genuinely happens offline (H3c's fix - see [`NullCsms`]'s doc comment):
    /// `NullCsms::authorize` always declines, so upstream falls back to the local authorization
    /// list, which starts empty and would reject an unlisted identifier - the connector would sit
    /// in `Locked` forever instead of reaching `Charging`. Seeding `"TAG-1"` into the list first
    /// (exactly the `ChargePointEvent::LocalListUpdated` a CSMS's `SendLocalList` would produce -
    /// see `tests/reservation_and_auth_list.rs`) is what makes this test's tag a genuinely
    /// authorized one rather than an arbitrary string that used to be accepted only because
    /// nothing ever checked.
    ///
    /// Times out rather than hanging forever if a transition never lands, so a regression fails
    /// the test instead of wedging the suite.
    async fn charge_locally(charger: &RunningCharger, evse_id: usize, connector_id: usize) {
        let mut states = charger.subscribe();

        charger
            .send(ChargePointEvent::LocalListUpdated {
                version: 1,
                entries: vec![LocalListEntry {
                    id_token: IdToken {
                        value: "TAG-1".into(),
                        kind: IdTokenKind::ISO14443,
                    },
                    status: AuthorizationStatus::Accepted,
                }],
            })
            .await
            .unwrap();

        charger
            .send(connector_event(
                evse_id,
                connector_id,
                ConnectorEvent::CableConnected,
            ))
            .await
            .unwrap();
        wait_for(
            &mut states,
            evse_id,
            connector_id,
            OcppConnectorState::Locked,
        )
        .await;

        charger
            .send(connector_event(
                evse_id,
                connector_id,
                ConnectorEvent::IdTokenPresented(IdToken {
                    value: "TAG-1".into(),
                    kind: IdTokenKind::ISO14443,
                }),
            ))
            .await
            .unwrap();
        wait_for(
            &mut states,
            evse_id,
            connector_id,
            OcppConnectorState::Charging,
        )
        .await;
    }

    async fn wait_for(
        states: &mut ocpp_charge_point::sync::WatchReceiver<
            ocpp_charge_point::state::ChargePointState,
        >,
        evse_id: usize,
        connector_id: usize,
        target: OcppConnectorState,
    ) {
        tokio::time::timeout(StdDuration::from_secs(5), async {
            loop {
                if states.borrow().evses[evse_id].connectors[connector_id] == target {
                    return;
                }
                states.changed().await;
            }
        })
        .await
        .unwrap_or_else(|_| panic!("connector never reached {target:?} within the timeout"));
    }

    #[tokio::test]
    async fn a_local_charger_has_no_registration_and_stays_offline_from_the_ocpp_state_alone() {
        let charger = start_local_charger(&config(vec![]), ChargerHardware::default()).await;
        assert_eq!(charger.state().registration, None);
    }

    #[tokio::test]
    async fn a_local_chargers_connector_reaches_charging_with_no_csms_at_all() {
        let charger = start_local_charger(
            &config(vec![EvseConfig {
                id: 1,
                connectors: 1,
            }]),
            ChargerHardware::default(),
        )
        .await;

        charge_locally(&charger, 0, 0).await;

        assert_eq!(
            charger.state().evses[0].connectors[0],
            OcppConnectorState::Charging
        );
    }

    /// H7: lock and contactor state live only on `FakeConnector`, not `ChargePointState`, so the
    /// only way to prove they reach `ConnectorState` is to drive a full local session (cable
    /// connected -> locked -> id token presented -> contactor closes once charging starts) through
    /// a real `RunningCharger` and read `ConnectorState` back through `apply_state`, rather than
    /// poking `FakeConnector` directly.
    #[tokio::test]
    async fn apply_state_surfaces_lock_and_contactor_reached_through_a_real_session() {
        let config = config(vec![EvseConfig {
            id: 1,
            connectors: 1,
        }]);
        let charger = start_local_charger(&config, ChargerHardware::default()).await;
        let mut state = ChargerState::from_config(config);

        // Before anything happens, both read their construction-time defaults.
        charger.apply_state(&mut state);
        assert!(!state.evses[0].connectors[0].locked);
        assert!(!state.evses[0].connectors[0].contactor_closed);

        charge_locally(&charger, 0, 0).await;

        charger.apply_state(&mut state);
        assert!(
            state.evses[0].connectors[0].locked,
            "the real hardware round trip locks the connector once a cable connects"
        );
        assert!(
            state.evses[0].connectors[0].contactor_closed,
            "the contactor closes once the session reaches Charging"
        );
    }

    #[tokio::test]
    async fn a_local_chargers_meter_accrues_energy_while_the_contactor_is_closed() {
        let charger = start_local_charger(
            &config(vec![EvseConfig {
                id: 1,
                connectors: 1,
            }]),
            ChargerHardware::default(),
        )
        .await;
        charge_locally(&charger, 0, 0).await;

        charger.tick(StdDuration::from_secs(3600)).await;

        let sample = charger.state().evses[0].latest_meter_samples[0]
            .expect("a sample should have been recorded once the contactor closed and ticked");
        assert!(
            sample.energy_wh > 0,
            "expected energy to accrue while charging, got {sample:?}"
        );
    }

    /// Mirrors `SimulatedMeter`'s own cadence-independence test (`charger/hardware/metering.rs`),
    /// but through the whole local integration - `RunningCharger::tick` down to
    /// `FakeChargePoint::tick` down to `SimulatedMeter::tick` - rather than the meter type alone.
    #[tokio::test]
    async fn the_same_total_elapsed_time_reads_the_same_whether_split_across_many_ticks_or_one() {
        let one_big_tick = start_local_charger(
            &config(vec![EvseConfig {
                id: 1,
                connectors: 1,
            }]),
            ChargerHardware::default(),
        )
        .await;
        charge_locally(&one_big_tick, 0, 0).await;
        one_big_tick.tick(StdDuration::from_secs(3600)).await;

        let many_small_ticks = start_local_charger(
            &config(vec![EvseConfig {
                id: 1,
                connectors: 1,
            }]),
            ChargerHardware::default(),
        )
        .await;
        charge_locally(&many_small_ticks, 0, 0).await;
        for _ in 0..100 {
            many_small_ticks
                .tick(StdDuration::from_millis(36_000))
                .await;
        }

        let big = one_big_tick.state().evses[0].latest_meter_samples[0]
            .unwrap()
            .energy_wh;
        let small = many_small_ticks.state().evses[0].latest_meter_samples[0]
            .unwrap()
            .energy_wh;
        assert!(
            (big - small).abs() <= 1,
            "one 3600s tick ({big} Wh) should match 100x36s ticks ({small} Wh)"
        );
    }

    #[tokio::test]
    async fn ticking_before_anything_is_charging_records_a_zero_sample_not_no_sample() {
        let charger = start_local_charger(
            &config(vec![EvseConfig {
                id: 1,
                connectors: 1,
            }]),
            ChargerHardware::default(),
        )
        .await;

        charger.tick(StdDuration::from_secs(60)).await;

        let sample = charger.state().evses[0].latest_meter_samples[0]
            .expect("idle hardware still measures and reports - Some(0), not None (H3b)");
        assert_eq!(sample.energy_wh, 0);
        assert_eq!(sample.power_w, Some(0));
        assert_eq!(sample.current_ma, Some(0));
    }

    // --- H3d: `start_local_charger` given a real bundle -----------------------------------------
    //
    // Before H3d, `start_local_charger` took no `ChargerHardware` at all and hardcoded `None` for
    // storage and display - a caller could not give a local charger persistence, a display, or
    // firmware/file-transfer hardware no matter what it passed, because there was nowhere to pass
    // it. These two tests drive the *public* entry point end to end with a real bundle (a tempdir
    // for storage, a real `FakeFirmwareInstaller`) to prove neither is silently dropped - unlike
    // `connect.rs`'s equivalent proof (which uses `RecordingCsms`/`RecordingStorage` at the
    // `register_setup_blocks` level), these go through `start_local_charger` by name.

    /// Persistence: a security event recorded on a local charger built with a real, tempdir-backed
    /// `FileStorage` must actually reach disk - not the `None` a hardcoded local path used to pass
    /// regardless of what `ChargerHardware` it was given. Reads the data back with a fresh
    /// `SecurityLogStore` over the same directory rather than through the running charger itself,
    /// so this is a genuine disk-content check, not a check that the in-memory handle merely exists.
    #[tokio::test]
    async fn a_local_charger_given_a_real_bundle_persists_a_security_event_to_disk() {
        let dir = tempfile::tempdir().expect("failed to create temp dir");
        let mut cfg = config(vec![]);
        cfg.capabilities = CapabilitiesConfig {
            has_persistent_storage: true,
            ..Default::default()
        };
        let hardware = ChargerHardware::new(dir.path());

        let charger = start_local_charger(&cfg, hardware).await;

        charger
            .send(ChargePointEvent::SecurityEventOccurred(SecurityEvent {
                event_type: SecurityEventType::TamperDetectionActivated,
                tech_info: Some("wired through, not dropped".into()),
            }))
            .await
            .expect("sending to a freshly-built runtime never fails");

        tokio::time::timeout(StdDuration::from_secs(5), async {
            loop {
                let restored = restore_security_log(
                    &SecurityEventLog::new(),
                    &SecurityLogStore::new(FileStorage::new(dir.path())),
                )
                .await;
                if restored > 0 {
                    return;
                }
                tokio::task::yield_now().await;
            }
        })
        .await
        .expect("the security event was never persisted to disk");
    }

    /// Firmware: `RunningCharger` must hold - and tick - the *same* `FakeFirmwareInstaller`
    /// `ChargerHardware` was given, not silently drop it. Driven with no CSMS at all
    /// (`install()` is called directly, the same public entry point `run_firmware_updates` itself
    /// calls internally): local mode never registers `firmware_updates` against a CSMS that
    /// doesn't exist (see `register_optional_hardware`'s `has_csms` doc comment), but the installer
    /// handle and its ticking are independent of whether a CSMS campaign ever drives it - installing
    /// firmware is local hardware behavior in its own right.
    #[tokio::test]
    async fn a_local_chargers_running_charger_ticks_a_firmware_installer_from_a_real_bundle() {
        let installer = Arc::new(FakeFirmwareInstaller::new(StdDuration::from_secs(90)));
        let hardware = ChargerHardware {
            firmware_installer: Some(Arc::clone(&installer)),
            ..Default::default()
        };

        let charger = start_local_charger(&config(vec![]), hardware).await;

        // `installer` is only cloned above - the original was moved into `hardware` and from
        // there into `RunningCharger`, so ticking through `charger` is the only way this
        // installation can ever progress. Ticks repeatedly rather than once, since a tick issued
        // before the spawned task has reached `install()` is a documented no-op (see
        // `FakeFirmwareInstaller::tick`'s own doc comment) - this simply keeps advancing simulated
        // time until one lands after that point.
        let install = tokio::spawn(async move { installer.install().await });
        tokio::time::timeout(StdDuration::from_secs(5), async {
            while !install.is_finished() {
                charger.tick(StdDuration::from_secs(90)).await;
                tokio::task::yield_now().await;
            }
        })
        .await
        .expect("the firmware install never completed through RunningCharger::tick");

        let outcome = install.await.expect("the install task panicked");
        assert!(
            matches!(
                outcome,
                Ok(ocpp_charge_point::hardware::FirmwareInstallOutcome::Installed)
            ),
            "expected the install to complete once RunningCharger::tick supplied enough \
             simulated time, got {outcome:?}"
        );
    }

    // --- H14b: `RunningCharger::set_discharging`/`exported_energy_wh` --------------------------

    /// The minimum the task asks for: flipping a connector into discharge programmatically (no
    /// OCPP path exists to do this - see `set_discharging`'s own doc comment) makes the meter's
    /// exported energy actually rise once `RunningCharger::tick` runs, exactly like H14a proved at
    /// `FakeConnector`'s own level - this proves the same thing reached through the whole local
    /// integration, the way `the_same_total_elapsed_time_reads_the_same_...` above does for
    /// import.
    #[tokio::test]
    async fn set_discharging_makes_the_meters_exported_energy_rise_through_tick() {
        let charger = start_local_charger(
            &config(vec![EvseConfig {
                id: 1,
                connectors: 1,
            }]),
            ChargerHardware::default(),
        )
        .await;
        charge_locally(&charger, 0, 0).await;

        assert_eq!(
            charger.exported_energy_wh(0, 0).unwrap(),
            0,
            "nothing has discharged yet"
        );

        charger.set_discharging(0, 0, true).unwrap();
        charger.tick(StdDuration::from_secs(3600)).await;

        assert!(
            charger.exported_energy_wh(0, 0).unwrap() > 0,
            "expected exported energy to rise once discharging and ticked"
        );
    }

    /// The cross-thread half of H7/H14 rendering: a frontend that can't call `apply_state` (it
    /// doesn't own this thread) forwards a snapshot instead, and that snapshot has to carry the same
    /// facts - including the two, direction and exported energy, that no `ChargePointState` snapshot
    /// contains at all.
    #[tokio::test]
    async fn a_hardware_snapshot_carries_what_apply_state_would_have_written() {
        let config = config(vec![EvseConfig {
            id: 1,
            connectors: 1,
        }]);
        let charger = start_local_charger(&config, ChargerHardware::default()).await;
        charge_locally(&charger, 0, 0).await;
        charger.set_discharging(0, 0, true).unwrap();
        charger.tick(StdDuration::from_secs(3600)).await;

        // Exactly what a frontend on another thread does with what it was forwarded: the OCPP
        // snapshot through `apply_ocpp_state`, the hardware snapshot through
        // `apply_hardware_snapshot` - which together must equal the single `apply_state` call a
        // caller on *this* thread would have made.
        let (ocpp, hardware) = (charger.state(), charger.hardware_snapshot());
        let mut from_snapshot = ChargerState::from_config(config.clone());
        apply_ocpp_state(&mut from_snapshot, &ocpp);
        apply_hardware_snapshot(&mut from_snapshot, &hardware);

        let mut from_apply_state = ChargerState::from_config(config);
        charger.apply_state(&mut from_apply_state);

        let connector = &from_snapshot.evses[0].connectors[0];
        assert!(
            connector.locked && connector.contactor_closed,
            "a connector mid-session is locked with its contactor closed: {connector:?}"
        );
        assert!(connector.discharging);
        assert_eq!(
            connector.exported_energy_wh,
            charger.exported_energy_wh(0, 0).unwrap()
        );
        assert_eq!(
            from_snapshot.evses, from_apply_state.evses,
            "forwarding a snapshot must project exactly what applying the hardware directly does"
        );
    }

    #[tokio::test]
    async fn set_discharging_reports_no_such_connector_rather_than_panicking_out_of_range() {
        let charger = start_local_charger(&config(vec![]), ChargerHardware::default()).await;

        assert_eq!(
            charger.set_discharging(0, 0, true),
            Err(NoSuchConnector {
                evse_id: 0,
                connector_id: 0
            })
        );
        assert_eq!(
            charger.exported_energy_wh(0, 0),
            Err(NoSuchConnector {
                evse_id: 0,
                connector_id: 0
            })
        );
    }

    // --- H13b: the key store, carried but never registered --------------------------------------

    /// Proves the key-store integration this task actually achieved: `ChargerHardware.key_store`
    /// round-trips through `start_local_charger` without being dropped or poisoned, and - the part
    /// that matters - the caller's own `Arc` clone, taken *before* handing `hardware` away, is
    /// still a live, independently usable `FileKeyStore<EcdsaCrypto>` afterward: it generates a
    /// real ECDSA key pair and signs with it, through the exact composition
    /// `crates/charge_point_simulator_core/src/charger/hardware/crypto.rs`'s own tests already
    /// prove works. `start_local_charger` never registers a `KeyStore` against anything (see
    /// `ChargerHardware`'s doc comment - no `ChargePointBuilder` method exists to register one
    /// against), so this only demonstrates the store is *usable*, not that it is wired into the
    /// running session - the honest limit of what this task could reach.
    #[tokio::test]
    async fn a_local_charger_given_a_real_bundle_still_leaves_the_callers_key_store_handle_usable()
    {
        use ocpp_charge_point::hardware::{KeyStore, SignatureAlgorithm};

        use super::super::hardware::{EcdsaCrypto, FileKeyStore};

        let dir = tempfile::tempdir().expect("failed to create temp dir");
        let key_store = Arc::new(FileKeyStore::new(
            FileStorage::new(dir.path()),
            EcdsaCrypto::new(),
        ));
        let hardware = ChargerHardware {
            key_store: Some(Arc::clone(&key_store)),
            ..Default::default()
        };

        let _charger = start_local_charger(&config(vec![]), hardware).await;

        let generated = key_store
            .generate_key_pair(SignatureAlgorithm::EcdsaP256Sha256)
            .await
            .expect("the caller's own Arc clone must still be a live, working key store");
        let signature = key_store
            .sign(&generated.handle, b"a digest the caller already hashed")
            .await
            .expect("signing through the caller's retained handle must still work");
        assert!(!signature.is_empty());
    }
}
