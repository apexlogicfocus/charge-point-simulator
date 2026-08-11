//! The shared result of actually running a charger's fake hardware against a real
//! `ocpp_charge_point` state machine - see [`RunningCharger`] and [`start_local_charger`], and
//! `docs/hardware-roadmap.md`'s H3b for why this module exists.

use std::ops::Deref;
use std::time::Duration;

use ocpp_charge_point::ChargePointBuilder;
use ocpp_charge_point::ChargePointRuntime;
use ocpp_charge_point::authorization::Authorizer;
use ocpp_charge_point::clock::SystemClock;
use ocpp_charge_point::executor::TokioExecutor;
use ocpp_charge_point::state::{AuthorizationStatus, IdToken};

use super::config::ChargerConfig;
use super::hardware::FakeChargePoint;

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
}

impl Deref for RunningCharger {
    type Target = ChargePointRuntime<FakeChargePoint>;

    fn deref(&self) -> &Self::Target {
        &self.runtime
    }
}

/// A trivial `Authorizer` that accepts every presented identifier without asking anyone - the
/// only sensible answer for a charger with no CSMS at all. Real hardware falls back to a local
/// authorization list or authorization cache when a CSMS is briefly unreachable (see
/// `ocpp_charge_point::authorization`'s module docs); a *local simulation* has neither and no
/// CSMS to have ever populated them, so unconditional acceptance is the offline behavior this
/// simulator can actually back - matching `ocpp-charge-point`'s own
/// `examples/simulated_charge_point.rs`, which notes that "a charge point whose backend is
/// unreachable still charges cars."
#[derive(Clone, Copy, Debug, Default)]
struct LocalAuthorizer;

#[async_trait::async_trait]
impl Authorizer for LocalAuthorizer {
    type Error = core::convert::Infallible;

    async fn authorize(&self, _id_token: &IdToken) -> Result<AuthorizationStatus, Self::Error> {
        Ok(AuthorizationStatus::Accepted)
    }
}

/// Starts a charger's fake hardware with no CSMS at all: no dial, no `register`/
/// `register_until_accepted` call - `ChargePointState::registration` stays `None` forever, and
/// [`super::ocpp_bridge::apply_ocpp_state`] reads that (via `SimulationMode::Local`) as
/// permanently `ConnectionStatus::Offline` rather than the endlessly-retried "booting" a real
/// CSMS's rejected/pending registration would mean.
///
/// Otherwise this runs the same machinery the connected path does: the same [`FakeChargePoint`],
/// the same `ChargePointBuilder`/`ChargePointRuntime`, the same connector state machine - a local
/// charger genuinely runs OCPP's connector lifecycle (cable connected, locked, authorizing,
/// charging, ...), it just has nowhere to report it. The one functional block registered is
/// Authorization, via [`LocalAuthorizer`] - without it, presenting an identifier would leave a
/// connector stuck in `Authorizing` forever, since nothing would ever answer the resulting
/// `AuthorizationRequested`. Every other functional block (persistence, display, smart charging,
/// ...) stays unregistered: they all either need a CSMS round trip this charger has nowhere to
/// send, or are separate opt-in tasks on `docs/hardware-roadmap.md` this one doesn't claim.
///
/// See `docs/hardware-roadmap.md`'s H3b for why this is the shape a local (unconnected)
/// simulation takes from here on, instead of the coarse, hardware-free state machine
/// `charger/state.rs`/`charger/command.rs` used to run entirely on their own.
pub async fn start_local_charger(config: &ChargerConfig) -> RunningCharger {
    let hardware = FakeChargePoint::from_config(config);
    let handle = hardware.clone();

    let builder = ChargePointBuilder::start(hardware, TokioExecutor)
        .await
        .unwrap_or_else(|error: core::convert::Infallible| match error {});
    let builder = builder.authorization(&LocalAuthorizer, SystemClock).await;

    RunningCharger::new(builder.build(), handle)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::charger::config::EvseConfig;
    use crate::charger::config::OcppVersion;
    use ocpp_charge_point::state::{
        ChargePointEvent, ConnectorEvent, ConnectorState as OcppConnectorState, EvseEvent,
        IdTokenKind,
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
    /// identifier presented, and authorized (automatic, via [`LocalAuthorizer`]) - the same
    /// sequence a real offline charger runs, with no CSMS anywhere in the loop. Times out rather
    /// than hanging forever if a transition never lands, so a regression fails the test instead
    /// of wedging the suite.
    async fn charge_locally(charger: &RunningCharger, evse_id: usize, connector_id: usize) {
        let mut states = charger.subscribe();

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
