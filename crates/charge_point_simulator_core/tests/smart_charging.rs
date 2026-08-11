//! Proves `docs/hardware-roadmap.md`'s H8 end to end: a CSMS-installed charging profile actually
//! drives `charge_point_simulator_core`'s simulated meter, through nothing but `core`'s
//! **public** API - see `CLAUDE.md`'s note that this crate is published to crates.io, so a test
//! that only reaches what a downstream consumer could reach is itself part of the proof.
//!
//! # `start_local_charger`, mostly (H3c)
//!
//! Before `docs/hardware-roadmap.md`'s H3c, `start_local_charger` registered exactly one
//! functional block (Authorization), so this file had to hand-build its own minimal charger
//! (`bespoke_start_charger`, below) through the same public `ChargePointBuilder` `RunningCharger`'s
//! two constructors use, just to get Smart Charging's two background projection loops
//! (`ocpp_charge_point::smart_charging::run_charging_limit_projection`/
//! `run_charging_limit_schedule`) actually running against a local, no-CSMS session. H3c wired
//! `start_local_charger` itself through `register_setup_blocks` - the same function
//! `connect_charger` uses - against a null CSMS, registering Smart Charging whenever
//! `capabilities.smart_charging` is declared, exactly as the connected path does. Four of this
//! file's five tests now use `start_local_charger` directly, with no bespoke chain at all - and,
//! since H3c also fixed local authorization to genuinely consult the local authorization list (see
//! `docs/hardware-roadmap.md`'s "Known gaps"), [`charge_locally`] below seeds a list entry before
//! presenting a tag, the same fallout `tests/reservation_and_auth_list.rs` and
//! `charger/running_charger.rs`'s own tests needed.
//!
//! # The one exception: `a_stepped_schedule_changes_the_applied_limit_at_the_period_boundary`
//!
//! `start_local_charger` hardcodes `ocpp_charge_point::clock::SystemClock` (the real wall clock)
//! for every registration that takes a `Clock`, including Smart Charging - there is no public way
//! to hand it a test-controlled one. The composite-schedule projection reads that clock to decide
//! which period of an *absolute* schedule (`ChargingSchedule::start_schedule`, anchored per
//! `amp_profile`'s doc comment to a fixed instant) is currently active. Three of the other four
//! tests get away with `start_local_charger`'s real clock because their schedules are unbounded
//! (`duration_secs: None`) and their anchor ([`epoch`]) is safely in the past relative to the real
//! wall clock, so "which period is active" only ever depends on "past or not", never on exactly
//! *how far* past. The boundary test can't: it needs `now` to land within specific, narrow windows
//! relative to `start_schedule` (`[0, 1800)` then `[1800, 3600)` seconds) to prove the limit
//! changes exactly at the crossing - not before, and not only once the test asks for the final
//! value - and the real wall clock cannot give it that without an actual 1800-second wait, which
//! decision 2 (`docs/hardware-roadmap.md`: never the wall clock in simulated behavior) rules out.
//!
//! So that one test keeps a bespoke chain (everything below prefixed `bespoke_`), built against
//! [`TestClock`], a `Clock` the test advances by hand in lockstep with [`FakeChargePoint::tick`].
//! **This is the finding H3c's brief asked to be reported rather than papered over**: `core` has
//! no public way to run a local charger against an injectable `Clock`/`Backoff`, so any test that
//! needs exact control over *when* something happens in real terms (rather than *how much
//! simulated time has elapsed*, which `RunningCharger::tick` already controls precisely) cannot be
//! expressed through `start_local_charger` alone yet.

use std::sync::{Arc, Mutex};
use std::time::Duration as StdDuration;

use chrono::{DateTime, Duration as ChronoDuration, Utc};

use charge_point_simulator_core::charger::{
    CapabilitiesConfig, ChargerConfig, ChargerState, EvseConfig, FakeChargePoint, OcppVersion,
    RunningCharger, apply_hardware_state, apply_ocpp_state, start_local_charger,
};
use ocpp_charge_point::ChargePointBuilder;
use ocpp_charge_point::ChargePointRuntime;
use ocpp_charge_point::actor::ChargePointActor;
use ocpp_charge_point::authorization::Authorizer;
use ocpp_charge_point::clock::Clock;
use ocpp_charge_point::executor::TokioExecutor;
use ocpp_charge_point::provisioning::Backoff;
use ocpp_charge_point::smart_charging::{
    ChargingLimitProjection, ClearChargingProfileHandler, GetCompositeScheduleHandler,
    SetChargingProfileHandler,
};
use ocpp_charge_point::state::{
    AuthorizationStatus, ChargePointEvent, ChargePointState, ChargingProfile,
    ChargingProfileCriteria, ChargingProfileId, ChargingProfileKind, ChargingProfilePurpose,
    ChargingProfileScope, ChargingRateUnit, ChargingSchedule, ChargingSchedulePeriod,
    ConnectorEvent, ConnectorState as OcppConnectorState, EvseEvent, IdToken, IdTokenKind,
    LocalListEntry,
};
use ocpp_charge_point::sync::WatchReceiver;

/// The hardware layer's simulated nominal charging rate (`SIMULATED_CHARGING_POWER_KW` in
/// `charger/hardware/metering.rs`) - not itself public, so mirrored here as the value an
/// unrestricted connector should accrue in one simulated hour. Used only as a sanity bound, with
/// generous tolerance, never as an exact equality.
const NOMINAL_HOURLY_KWH: f64 = 7.4;

/// Builds a `ChargerConfig` for one EVSE with a single connector, declaring
/// `capabilities.smart_charging` per `smart_charging`.
fn config(smart_charging: bool) -> ChargerConfig {
    ChargerConfig {
        id: "CP001".into(),
        ocpp_version: OcppVersion::V21,
        evses: vec![EvseConfig {
            id: 1,
            connectors: 1,
        }],
        has_display: false,
        capabilities: CapabilitiesConfig {
            smart_charging,
            ..Default::default()
        },
    }
}

fn connector_event(evse_id: usize, connector_id: usize, event: ConnectorEvent) -> ChargePointEvent {
    ChargePointEvent::Evse {
        evse_id,
        event: EvseEvent::Connector {
            connector_id,
            event,
        },
    }
}

async fn wait_for_connector_state(
    states: &mut WatchReceiver<ChargePointState>,
    evse_id: usize,
    connector_id: usize,
    target: OcppConnectorState,
) {
    tokio::time::timeout(StdDuration::from_secs(5), async {
        loop {
            if states.borrow().evses[evse_id].connectors[connector_id] == target {
                return;
            }
            let _ = states.changed().await;
        }
    })
    .await
    .unwrap_or_else(|_| panic!("connector never reached {target:?} within the timeout"));
}

async fn install_profile(runtime: &ChargePointRuntime<FakeChargePoint>, profile: ChargingProfile) {
    runtime
        .send(ChargePointEvent::ChargingProfileSet {
            scope: ChargingProfileScope::ChargePoint,
            profile: Box::new(profile),
        })
        .await
        .unwrap();
}

async fn clear_all_profiles(runtime: &ChargePointRuntime<FakeChargePoint>) {
    runtime
        .send(ChargePointEvent::ChargingProfilesCleared {
            criteria: ChargingProfileCriteria::default(),
        })
        .await
        .unwrap();
}

// ---------------------------------------------------------------------------------------------
// Charging profile fixtures
// ---------------------------------------------------------------------------------------------

/// A `TxDefault` profile (applies to any transaction with no `Tx` profile of its own - the common
/// case) scoped to the whole charge point, with one amp-denominated schedule anchored at
/// `start_schedule` (never left `None`: an absolute schedule with no `start_schedule` anchors to
/// whatever instant it happens to first be evaluated at - see
/// `ocpp_charge_point::smart_charging`'s `schedule_start_instant` - which would make period
/// boundaries drift with evaluation timing instead of landing where the test expects).
fn amp_profile(
    id: i32,
    start_schedule: DateTime<Utc>,
    duration_secs: Option<u32>,
    periods: Vec<ChargingSchedulePeriod>,
) -> ChargingProfile {
    ChargingProfile {
        id: ChargingProfileId(id),
        stack_level: 0,
        purpose: ChargingProfilePurpose::TxDefault,
        kind: ChargingProfileKind::Absolute,
        recurrency: None,
        valid_from: None,
        valid_to: None,
        transaction_id: None,
        schedules: vec![ChargingSchedule {
            id: 1,
            start_schedule: Some(start_schedule),
            duration_secs,
            rate_unit: ChargingRateUnit::Amps,
            min_charging_rate: None,
            periods,
        }],
        dyn_update_interval_secs: None,
        dyn_update_time: None,
    }
}

fn flat_period(limit_amps: f64) -> ChargingSchedulePeriod {
    ChargingSchedulePeriod {
        start_period_secs: 0,
        limit: limit_amps,
        number_phases: None,
    }
}

/// An arbitrary, fixed reference instant - never the real wall clock - every schedule anchors its
/// `start_schedule` to (and, for the bespoke-chain test, [`TestClock`] as well). Safely in the
/// past relative to the real `SystemClock` `start_local_charger` uses, which is what lets the
/// unbounded-duration tests below use it without needing a controllable clock at all.
fn epoch() -> DateTime<Utc> {
    DateTime::from_timestamp(1_700_000_000, 0).expect("a valid fixed Unix timestamp")
}

// ---------------------------------------------------------------------------------------------
// The `start_local_charger` path - used by every test but the period-boundary one (see module doc)
// ---------------------------------------------------------------------------------------------

/// Drives `charger` through a full local session up to `Charging`: seeds `"TAG-1"` into the local
/// authorization list (H3c made authorization genuinely offline - see the module doc comment -
/// so an unlisted identifier would now be rejected, leaving the connector `Locked` forever), then
/// cable connected, locked, identifier presented, authorized from the list. Times out rather than
/// hanging forever if a transition never lands.
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
    wait_for_connector_state(
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
    wait_for_connector_state(
        &mut states,
        evse_id,
        connector_id,
        OcppConnectorState::Charging,
    )
    .await;
}

/// Projects `charger`'s full observable state onto a fresh [`ChargerState`] via
/// [`RunningCharger::apply_state`] - the same public entry point a downstream frontend uses.
fn read_state(charger: &RunningCharger, config: &ChargerConfig) -> ChargerState {
    let mut state = ChargerState::from_config(config.clone());
    charger.apply_state(&mut state);
    state
}

/// Polls (via the state watch, never a `sleep`) until `ConnectorState::current_limit_ma` for
/// `evse_id`/`connector_id` reads `expected`, or panics after a timeout. The limit's path to
/// hardware crosses two async hops after any single event this test sends - the projection loop
/// noticing a state change and computing a limit, then the spawned hardware-command loop in
/// `FakeChargePoint::start` actually calling `Connector::set_current_limit` - so nothing about it
/// is synchronous with `charger.send(..).await` returning, and polling is the correct tool per
/// `charger/running_charger.rs`'s own `wait_for`.
async fn wait_for_current_limit(
    charger: &RunningCharger,
    config: &ChargerConfig,
    evse_id: usize,
    connector_id: usize,
    expected: Option<u32>,
) -> ChargerState {
    let mut states = charger.subscribe();
    tokio::time::timeout(StdDuration::from_secs(5), async {
        loop {
            let state = read_state(charger, config);
            if state.evses[evse_id].connectors[connector_id].current_limit_ma == expected {
                return state;
            }
            let _ = states.changed().await;
        }
    })
    .await
    .unwrap_or_else(|_| panic!("current limit never reached {expected:?} within the timeout"))
}

// ---------------------------------------------------------------------------------------------
// The bespoke chain - only for `a_stepped_schedule_changes_the_applied_limit_at_the_period_boundary`
// (see the module doc comment's "The one exception" section for why)
// ---------------------------------------------------------------------------------------------

/// A [`Clock`] the test advances by hand, never the wall clock (decision 2,
/// `docs/hardware-roadmap.md`). Advanced by the test in lockstep with [`FakeChargePoint::tick`] so
/// a charging schedule's period boundaries are crossed exactly when the test ticks past them -
/// control `start_local_charger`'s hardcoded `SystemClock` cannot offer (see the module doc
/// comment).
#[derive(Clone)]
struct TestClock(Arc<Mutex<DateTime<Utc>>>);

impl TestClock {
    fn new(start: DateTime<Utc>) -> Self {
        Self(Arc::new(Mutex::new(start)))
    }

    fn now_value(&self) -> DateTime<Utc> {
        *self.0.lock().expect("lock poisoned")
    }

    fn advance(&self, elapsed: StdDuration) {
        let mut now = self.0.lock().expect("lock poisoned");
        *now += ChronoDuration::from_std(elapsed).expect("elapsed fits in a chrono::Duration");
    }
}

impl Clock for TestClock {
    fn now(&self) -> DateTime<Utc> {
        self.now_value()
    }
}

/// The only sensible `Authorizer` for a charger with no CSMS - see
/// `charger/running_charger.rs`'s private `NullCsms::authorize` for the production equivalent
/// (which declines, so real local mode falls back to the local authorization list). Reproduced
/// here as an always-accepting stand-in because the bespoke chain has no local authorization list
/// story to exercise and isn't the point of this test - only exact period-boundary timing is.
#[derive(Clone, Copy, Debug, Default)]
struct BespokeAuthorizer;

#[async_trait::async_trait]
impl Authorizer for BespokeAuthorizer {
    type Error = core::convert::Infallible;

    async fn authorize(&self, _id_token: &IdToken) -> Result<AuthorizationStatus, Self::Error> {
        Ok(AuthorizationStatus::Accepted)
    }
}

/// A CSMS stand-in that registers nothing anywhere - the three methods `ChargePointBuilder::
/// smart_charging` calls exist only to hand this charge point's inbound `SetChargingProfile`/
/// `ClearChargingProfile`/`GetCompositeSchedule` handling to a real network client, and this
/// charger has no CSMS at all. Every profile in this test is installed directly as a
/// `ChargePointEvent::ChargingProfileSet`, bypassing the handler this would otherwise register -
/// so nothing here ever needs to be reached, only registered (harmlessly, as a no-op) so that
/// `smart_charging`'s other job - spawning the two projection loops that turn a stored profile
/// into `HardwareCommand::SetCurrentLimit` - actually happens.
#[derive(Clone, Copy, Debug, Default)]
struct BespokeNoopCsms;

#[async_trait::async_trait]
impl SetChargingProfileHandler for BespokeNoopCsms {
    async fn register_set_charging_profile_handler(&self, _actor: ChargePointActor) {}
}

#[async_trait::async_trait]
impl ClearChargingProfileHandler for BespokeNoopCsms {
    async fn register_clear_charging_profile_handler(&self, _actor: ChargePointActor) {}
}

#[async_trait::async_trait]
impl GetCompositeScheduleHandler for BespokeNoopCsms {
    async fn register_get_composite_schedule_handler(
        &self,
        _actor: ChargePointActor,
        _projection: Arc<ChargingLimitProjection>,
    ) {
    }
}

/// The period-boundary loop (`run_charging_limit_schedule`) sleeps on this between evaluations.
/// A real `Backoff` would sleep for however long is left until the next schedule boundary
/// (`ocpp_charge_point::smart_charging::projection`'s own doc comment), which is meaningless
/// against a [`TestClock`] the test itself steps by hand - so this ignores `seconds` entirely and
/// re-evaluates on a short, fixed real-time cadence instead. That keeps the loop from busy
/// spinning while still converging quickly whenever the test advances the clock or ticks the
/// meter, without ever depending on the wall clock for correctness (only for polling cadence).
#[derive(Clone, Copy, Debug, Default)]
struct BespokeFastBackoff;

#[async_trait::async_trait]
impl Backoff for BespokeFastBackoff {
    async fn wait(&self, _seconds: u32) {
        tokio::time::sleep(StdDuration::from_millis(5)).await;
    }
}

/// Starts a local (no-CSMS) charger the same shape `start_local_charger` builds (see its own doc
/// comment), but against a caller-supplied [`TestClock`] instead of the real `SystemClock` -
/// exactly the one thing `start_local_charger` cannot be asked for yet (see the module doc
/// comment). Registers Authorization against [`BespokeAuthorizer`] and - only when `config`
/// declares `capabilities.smart_charging`, mirroring `register_setup_blocks`'s own gate - Smart
/// Charging against [`BespokeNoopCsms`]. Returns the runtime plus a live [`FakeChargePoint`]
/// handle to tick (see `charger/running_charger.rs`'s doc comment on why a handle has to be cloned
/// out before the other clone's ownership moves into the builder).
async fn bespoke_start_charger(
    config: &ChargerConfig,
    clock: TestClock,
) -> (ChargePointRuntime<FakeChargePoint>, FakeChargePoint) {
    let hardware = FakeChargePoint::from_config(config);
    let handle = hardware.clone();

    let builder = ChargePointBuilder::start(hardware, TokioExecutor)
        .await
        .unwrap_or_else(|error: core::convert::Infallible| match error {});
    let builder = builder
        .authorization(&BespokeAuthorizer, clock.clone())
        .await;
    let builder = if config.capabilities().smart_charging {
        builder
            .smart_charging(
                &BespokeNoopCsms,
                Arc::new(ChargingLimitProjection::new()),
                clock,
                BespokeFastBackoff,
            )
            .await
    } else {
        builder
    };

    (builder.build(), handle)
}

/// Drives `runtime` through a full local session up to `Charging` against [`BespokeAuthorizer`],
/// which accepts unconditionally - no local-authorization-list seeding needed here, unlike
/// [`charge_locally`].
async fn bespoke_charge_locally(
    runtime: &ChargePointRuntime<FakeChargePoint>,
    evse_id: usize,
    connector_id: usize,
) {
    let mut states = runtime.subscribe();

    runtime
        .send(connector_event(
            evse_id,
            connector_id,
            ConnectorEvent::CableConnected,
        ))
        .await
        .unwrap();
    wait_for_connector_state(
        &mut states,
        evse_id,
        connector_id,
        OcppConnectorState::Locked,
    )
    .await;

    runtime
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
    wait_for_connector_state(
        &mut states,
        evse_id,
        connector_id,
        OcppConnectorState::Charging,
    )
    .await;
}

/// Projects both `runtime`'s OCPP state and `hardware`'s hardware-only state (lock, contactor,
/// current limit - H7) onto a fresh [`ChargerState`], the same two-call sequence
/// `RunningCharger::apply_state` uses internally - reproduced here because the bespoke chain has
/// no `RunningCharger` to hand (it has no public constructor outside `charger/`).
fn bespoke_read_state(
    runtime: &ChargePointRuntime<FakeChargePoint>,
    hardware: &FakeChargePoint,
    config: &ChargerConfig,
) -> ChargerState {
    let mut state = ChargerState::from_config(config.clone());
    apply_ocpp_state(&mut state, &runtime.state());
    apply_hardware_state(&mut state, hardware);
    state
}

/// [`wait_for_current_limit`], against the bespoke chain's separate runtime/hardware handles.
async fn bespoke_wait_for_current_limit(
    runtime: &ChargePointRuntime<FakeChargePoint>,
    hardware: &FakeChargePoint,
    config: &ChargerConfig,
    evse_id: usize,
    connector_id: usize,
    expected: Option<u32>,
) -> ChargerState {
    let mut states = runtime.subscribe();
    tokio::time::timeout(StdDuration::from_secs(5), async {
        loop {
            let state = bespoke_read_state(runtime, hardware, config);
            if state.evses[evse_id].connectors[connector_id].current_limit_ma == expected {
                return state;
            }
            let _ = states.changed().await;
        }
    })
    .await
    .unwrap_or_else(|_| panic!("current limit never reached {expected:?} within the timeout"))
}

// ---------------------------------------------------------------------------------------------
// The chain, proven
// ---------------------------------------------------------------------------------------------

/// The headline claim: a CSMS-installed profile with a restrictive limit measurably slows energy
/// accrual relative to an identical charger with no profile at all, over the same simulated
/// elapsed time. This is the whole chain at once - `ChargingProfileSet` -> composite schedule ->
/// `CurrentLimitComputed` -> `HardwareCommand::SetCurrentLimit` -> `FakeConnector::
/// set_current_limit` -> `SimulatedMeter::tick`'s clamp - collapsed into one observable number.
#[tokio::test]
async fn a_restrictive_profile_accrues_less_energy_than_no_profile_over_the_same_elapsed_time() {
    let cfg = config(true);

    let unrestricted = start_local_charger(&cfg).await;
    charge_locally(&unrestricted, 0, 0).await;

    let restricted = start_local_charger(&cfg).await;
    charge_locally(&restricted, 0, 0).await;
    install_profile(
        &restricted,
        amp_profile(1, epoch(), None, vec![flat_period(8.0)]),
    )
    .await;
    wait_for_current_limit(&restricted, &cfg, 0, 0, Some(8_000)).await;

    unrestricted.tick(StdDuration::from_secs(3_600)).await;
    restricted.tick(StdDuration::from_secs(3_600)).await;

    let unrestricted_energy = read_state(&unrestricted, &cfg).evses[0].metrics.energy_kwh;
    let restricted_energy = read_state(&restricted, &cfg).evses[0].metrics.energy_kwh;

    assert!(
        restricted_energy > 0.0,
        "the restricted charger should still accrue some energy, got {restricted_energy}"
    );
    assert!(
        restricted_energy < unrestricted_energy,
        "restricted ({restricted_energy} kWh) should be less than unrestricted \
         ({unrestricted_energy} kWh) over the same elapsed time"
    );
}

/// `Connector::set_current_limit`'s docs single out `Some(0)` as "suspend charging", distinct
/// from `None`'s "no limit" - a zero-amp period must halt accrual without ending the transaction
/// or faulting the connector.
#[tokio::test]
async fn a_zero_limit_suspends_accrual_without_ending_the_transaction_or_faulting() {
    let cfg = config(true);
    let charger = start_local_charger(&cfg).await;
    charge_locally(&charger, 0, 0).await;

    install_profile(
        &charger,
        amp_profile(1, epoch(), None, vec![flat_period(0.0)]),
    )
    .await;
    wait_for_current_limit(&charger, &cfg, 0, 0, Some(0)).await;

    charger.tick(StdDuration::from_secs(3_600)).await;

    let state = read_state(&charger, &cfg);
    assert_eq!(
        state.evses[0].metrics.energy_kwh, 0.0,
        "a Some(0) limit should halt accrual entirely"
    );
    assert_eq!(
        charger.state().evses[0].connectors[0],
        OcppConnectorState::Charging,
        "suspended by a zero limit, not ended - the connector must stay Charging"
    );
    assert!(
        state.evses[0].connectors[0].contactor_closed,
        "a suspended limit is not a fault - the contactor stays closed"
    );
}

/// `ChargingProfilesCleared` is the "limit removed" path - `Connector::set_current_limit`'s
/// `None`, distinct from `Some(0)` - and must restore the unrestricted rate rather than leaving
/// the last-applied limit stuck.
#[tokio::test]
async fn clearing_the_profile_restores_the_unrestricted_rate() {
    let cfg = config(true);
    let charger = start_local_charger(&cfg).await;
    charge_locally(&charger, 0, 0).await;

    install_profile(
        &charger,
        amp_profile(1, epoch(), None, vec![flat_period(8.0)]),
    )
    .await;
    wait_for_current_limit(&charger, &cfg, 0, 0, Some(8_000)).await;

    charger.tick(StdDuration::from_secs(3_600)).await;
    let limited_energy = read_state(&charger, &cfg).evses[0].metrics.energy_kwh;
    assert!(
        limited_energy > 0.0 && limited_energy < NOMINAL_HOURLY_KWH,
        "the limited hour should accrue less than the nominal rate, got {limited_energy}"
    );

    clear_all_profiles(&charger).await;
    wait_for_current_limit(&charger, &cfg, 0, 0, None).await;

    charger.tick(StdDuration::from_secs(3_600)).await;
    let restored_energy = read_state(&charger, &cfg).evses[0].metrics.energy_kwh;

    let second_hour = restored_energy - limited_energy;
    assert!(
        (second_hour - NOMINAL_HOURLY_KWH).abs() < 0.05,
        "the second hour, with the limit cleared, should accrue a full nominal hour \
         ({NOMINAL_HOURLY_KWH} kWh), got {second_hour}"
    );
}

/// A stepped schedule must change the *applied* limit exactly when simulated time crosses a
/// period boundary - not before, and not only once the test asks for the final value.
///
/// The one test in this file still on the bespoke chain - see the module doc comment's "The one
/// exception" section for why `start_local_charger`'s hardcoded `SystemClock` cannot express this.
#[tokio::test]
async fn a_stepped_schedule_changes_the_applied_limit_at_the_period_boundary() {
    let cfg = config(true);
    let clock = TestClock::new(epoch());
    let (runtime, hardware) = bespoke_start_charger(&cfg, clock.clone()).await;
    bespoke_charge_locally(&runtime, 0, 0).await;

    install_profile(
        &runtime,
        amp_profile(
            1,
            epoch(),
            Some(3_600),
            vec![
                ChargingSchedulePeriod {
                    start_period_secs: 0,
                    limit: 8.0,
                    number_phases: None,
                },
                ChargingSchedulePeriod {
                    start_period_secs: 1_800,
                    limit: 16.0,
                    number_phases: None,
                },
            ],
        ),
    )
    .await;
    bespoke_wait_for_current_limit(&runtime, &hardware, &cfg, 0, 0, Some(8_000)).await;

    // Not yet at the boundary: still the first period's limit.
    clock.advance(StdDuration::from_secs(900));
    hardware.tick(StdDuration::from_secs(900)).await;
    let state = bespoke_read_state(&runtime, &hardware, &cfg);
    assert_eq!(
        state.evses[0].connectors[0].current_limit_ma,
        Some(8_000),
        "short of the 1800s boundary, the limit should not have changed yet"
    );

    // Cross the boundary.
    clock.advance(StdDuration::from_secs(900));
    hardware.tick(StdDuration::from_secs(900)).await;
    bespoke_wait_for_current_limit(&runtime, &hardware, &cfg, 0, 0, Some(16_000)).await;
}

/// Capabilities gate *behavior*, not just advertisement (per `charger/config.rs`'s
/// `CapabilitiesConfig` docs and the hardware roadmap's "never advertise what isn't simulated"
/// principle): a charger that never declares `smart_charging` - and so never has the Smart
/// Charging functional block registered, mirroring `connect_charger`'s own capability gate -
/// must be unaffected by an installed profile, even though the profile still lands in
/// `ChargePointState::charging_profiles` (installing one isn't itself gated - only the
/// projection that would ever act on it is).
#[tokio::test]
async fn a_charger_without_the_smart_charging_capability_is_unaffected_by_an_installed_profile() {
    let cfg = config(false);
    let charger = start_local_charger(&cfg).await;
    charge_locally(&charger, 0, 0).await;

    install_profile(
        &charger,
        amp_profile(1, epoch(), None, vec![flat_period(8.0)]),
    )
    .await;

    charger.tick(StdDuration::from_secs(3_600)).await;

    let state = read_state(&charger, &cfg);
    assert_eq!(
        state.evses[0].connectors[0].current_limit_ma, None,
        "with no smart_charging capability, nothing ever computes or applies a limit"
    );
    assert!(
        (state.evses[0].metrics.energy_kwh - NOMINAL_HOURLY_KWH).abs() < 0.05,
        "energy should accrue at the full nominal rate - the profile has no path to hardware \
         at all - got {}",
        state.evses[0].metrics.energy_kwh
    );
}
