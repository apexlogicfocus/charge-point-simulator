//! The shared result of actually running a charger's fake hardware against a real
//! `ocpp_charge_point` state machine - see [`RunningCharger`] and [`start_local_charger`], and
//! `docs/hardware-roadmap.md`'s H3b for why this module exists.

use std::ops::Deref;
use std::time::Duration;

use ocpp_charge_point::ChargePointBuilder;
use ocpp_charge_point::ChargePointRuntime;
use ocpp_charge_point::clock::{SystemClock, SystemMonotonicClock};
use ocpp_charge_point::executor::TokioExecutor;
use ocpp_charge_point::provisioning::TokioBackoff;

use super::config::ChargerConfig;
use super::connect::{NullCsms, register_setup_blocks};
use super::hardware::{FakeChargePoint, FakeDisplay, FileStorage};
use super::ocpp_bridge::{apply_hardware_state, apply_ocpp_state};
use super::state::ChargerState;

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
pub struct RunningCharger {
    runtime: ChargePointRuntime<FakeChargePoint>,
    hardware: FakeChargePoint,
}

impl RunningCharger {
    pub(crate) fn new(
        runtime: ChargePointRuntime<FakeChargePoint>,
        hardware: FakeChargePoint,
    ) -> Self {
        Self { runtime, hardware }
    }

    /// Advances the simulated hardware clock by `elapsed` - see [`FakeChargePoint::tick`]. The
    /// only entry point through which this charger's meter ever moves, for a local charger and a
    /// live-CSMS one alike (decision 2 in `docs/hardware-roadmap.md`: simulated time only,
    /// injected by the caller, never a `tokio::time::interval` owned in here).
    pub async fn tick(&self, elapsed: Duration) {
        self.hardware.tick(elapsed).await;
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
/// `local_authorization_list`, and `smart_charging` (+`charging_profile_reports`, so a locally
/// installed `ChargingProfileSet` actually computes and applies a current limit - H8's gap).
/// [`register_setup_blocks`]'s own `has_csms` doc comment has the full registered/skipped list and
/// the reasoning behind each entry.
///
/// See `docs/hardware-roadmap.md`'s H3b for why this is the shape a local (unconnected)
/// simulation takes at all, instead of the coarse, hardware-free state machine
/// `charger/state.rs`/`charger/command.rs` used to run entirely on their own.
pub async fn start_local_charger(config: &ChargerConfig) -> RunningCharger {
    let hardware = FakeChargePoint::from_config(config);
    let handle = hardware.clone();

    let builder = ChargePointBuilder::start(hardware, TokioExecutor)
        .await
        .unwrap_or_else(|error: core::convert::Infallible| match error {});
    let builder = register_setup_blocks(
        builder,
        &NullCsms,
        TokioBackoff,
        SystemMonotonicClock,
        SystemClock,
        None::<&FileStorage>,
        None::<FakeDisplay>,
        false, // has_csms: no CSMS is ever dialed in local mode.
    )
    .await;

    RunningCharger::new(builder.build(), handle)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::charger::config::EvseConfig;
    use crate::charger::config::OcppVersion;
    use ocpp_charge_point::state::{
        AuthorizationStatus, ChargePointEvent, ConnectorEvent,
        ConnectorState as OcppConnectorState, EvseEvent, IdToken, IdTokenKind, LocalListEntry,
    };
    use std::time::Duration as StdDuration;

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
        let charger = start_local_charger(&config(vec![])).await;
        assert_eq!(charger.state().registration, None);
    }

    #[tokio::test]
    async fn a_local_chargers_connector_reaches_charging_with_no_csms_at_all() {
        let charger = start_local_charger(&config(vec![EvseConfig {
            id: 1,
            connectors: 1,
        }]))
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
        let charger = start_local_charger(&config).await;
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
        let charger = start_local_charger(&config(vec![EvseConfig {
            id: 1,
            connectors: 1,
        }]))
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
        let one_big_tick = start_local_charger(&config(vec![EvseConfig {
            id: 1,
            connectors: 1,
        }]))
        .await;
        charge_locally(&one_big_tick, 0, 0).await;
        one_big_tick.tick(StdDuration::from_secs(3600)).await;

        let many_small_ticks = start_local_charger(&config(vec![EvseConfig {
            id: 1,
            connectors: 1,
        }]))
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
        let charger = start_local_charger(&config(vec![EvseConfig {
            id: 1,
            connectors: 1,
        }]))
        .await;

        charger.tick(StdDuration::from_secs(60)).await;

        let sample = charger.state().evses[0].latest_meter_samples[0]
            .expect("idle hardware still measures and reports - Some(0), not None (H3b)");
        assert_eq!(sample.energy_wh, 0);
        assert_eq!(sample.power_w, Some(0));
        assert_eq!(sample.current_ma, Some(0));
    }
}
