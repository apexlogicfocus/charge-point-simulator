//! Proves `docs/hardware-roadmap.md`'s H8 end to end: a CSMS-installed charging profile actually
//! drives `charge_point_simulator_core`'s simulated meter, through nothing but `core`'s
//! **public** API - see `CLAUDE.md`'s note that this crate is published to crates.io, so a test
//! that only reaches what a downstream consumer could reach is itself part of the proof.
//!
//! # Why this isn't just `start_local_charger` + `send`
//!
//! The obvious shape - take the `RunningCharger` from `start_local_charger`, `send` a
//! `ChargePointEvent::ChargingProfileSet` at it, tick it, done - does not exercise the whole
//! chain. `start_local_charger` (`charger/running_charger.rs`) registers exactly one functional
//! block, Authorization; it never calls `ocpp_charge_point::ChargePointBuilder::smart_charging`,
//! which is what spawns the two background loops
//! (`ocpp_charge_point::smart_charging::run_charging_limit_projection`/
//! `run_charging_limit_schedule`) that turn an installed profile into a composite schedule and
//! push `ConnectorEvent::CurrentLimitComputed` at the state machine. Without that registration, a
//! `ChargingProfileSet` event lands in `ChargePointState::charging_profiles` and then goes
//! nowhere - nothing ever computes a limit from it, so `HardwareCommand::SetCurrentLimit` never
//! fires and the meter never clamps. `connect_charger`'s `register_setup_blocks` does call
//! `smart_charging` (gated on `Capabilities::smart_charging`, per H2/H8), but only for a charger
//! dialed against a real CSMS.
//!
//! So this file builds its own minimal local charger (`start_charger` below), through the same
//! public `ChargePointBuilder` used by `RunningCharger`'s two constructors, registering
//! Authorization exactly as `start_local_charger` does *and* - only when the config declares
//! `capabilities.smart_charging`, mirroring `register_setup_blocks`'s own gate - Smart Charging
//! too, against a trivial no-op fake CSMS (`NoopCsms`) satisfying the three handler-registration
//! traits `smart_charging` requires. Every type involved (`ChargePointBuilder`,
//! `ChargePointRuntime`, `ocpp_charge_point::authorization::Authorizer`,
//! `ocpp_charge_point::smart_charging::{SetChargingProfileHandler, ClearChargingProfileHandler,
//! GetCompositeScheduleHandler}`, `ocpp_charge_point::provisioning::Backoff`,
//! `ocpp_charge_point::clock::Clock`) is public on `ocpp_charge_point`, which is itself a regular
//! (non-dev) dependency of `charge_point_simulator_core`, so naming it here needs no additions to
//! this crate's `Cargo.toml`.
//!
//! Simulated time only, per decision 2 in the hardware roadmap: `TestClock` below is a
//! caller-advanced `Clock`, never the wall clock, kept in lockstep with every
//! `FakeChargePoint::tick` call so a schedule's period boundaries land exactly where the test
//! expects them to.

use std::sync::{Arc, Mutex};
use std::time::Duration as StdDuration;

use chrono::{DateTime, Duration as ChronoDuration, Utc};

use charge_point_simulator_core::charger::{
    CapabilitiesConfig, ChargerConfig, ChargerState, EvseConfig, FakeChargePoint, OcppVersion,
    apply_hardware_state, apply_ocpp_state,
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
};
use ocpp_charge_point::sync::WatchReceiver;

/// The hardware layer's simulated nominal charging rate (`SIMULATED_CHARGING_POWER_KW` in
/// `charger/hardware/metering.rs`) - not itself public, so mirrored here as the value an
/// unrestricted connector should accrue in one simulated hour. Used only as a sanity bound, with
/// generous tolerance, never as an exact equality.
const NOMINAL_HOURLY_KWH: f64 = 7.4;

// ---------------------------------------------------------------------------------------------
// Simulated clock
// ---------------------------------------------------------------------------------------------

/// A [`Clock`] the test advances by hand, never the wall clock (decision 2,
/// `docs/hardware-roadmap.md`). Shared (via `Arc`) between every registration on one charger that
/// needs a `Clock`, and advanced by the test in lockstep with [`FakeChargePoint::tick`] so a
/// charging schedule's period boundaries are crossed exactly when the test ticks past them.
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

/// An arbitrary, fixed reference instant - never the real wall clock - every test anchors its
/// [`TestClock`] and its charging schedules' `start_schedule` to.
fn epoch() -> DateTime<Utc> {
    DateTime::from_timestamp(1_700_000_000, 0).expect("a valid fixed Unix timestamp")
}

// ---------------------------------------------------------------------------------------------
// A local charger with an optional Smart Charging registration
// ---------------------------------------------------------------------------------------------

/// The only sensible `Authorizer` for a charger with no CSMS - see
/// `charger/running_charger.rs`'s private `LocalAuthorizer`, reproduced here because this file
/// builds its own charger rather than going through `start_local_charger` (see this module's
/// doc comment for why) and that type isn't public.
#[derive(Clone, Copy, Debug, Default)]
struct LocalAuthorizer;

#[async_trait::async_trait]
impl Authorizer for LocalAuthorizer {
    type Error = core::convert::Infallible;

    async fn authorize(&self, _id_token: &IdToken) -> Result<AuthorizationStatus, Self::Error> {
        Ok(AuthorizationStatus::Accepted)
    }
}

/// A CSMS stand-in that registers nothing anywhere - the three methods `ChargePointBuilder::
/// smart_charging` calls exist only to hand this charge point's inbound `SetChargingProfile`/
/// `ClearChargingProfile`/`GetCompositeSchedule` handling to a real network client, and this
/// charger has no CSMS at all. Every profile in these tests is installed directly as a
/// `ChargePointEvent::ChargingProfileSet`, bypassing the handler this would otherwise register -
/// so nothing here ever needs to be reached, only registered (harmlessly, as a no-op) so that
/// `smart_charging`'s other job - spawning the two projection loops that turn a stored profile
/// into `HardwareCommand::SetCurrentLimit` - actually happens.
#[derive(Clone, Copy, Debug, Default)]
struct NoopCsms;

#[async_trait::async_trait]
impl SetChargingProfileHandler for NoopCsms {
    async fn register_set_charging_profile_handler(&self, _actor: ChargePointActor) {}
}

#[async_trait::async_trait]
impl ClearChargingProfileHandler for NoopCsms {
    async fn register_clear_charging_profile_handler(&self, _actor: ChargePointActor) {}
}

#[async_trait::async_trait]
impl GetCompositeScheduleHandler for NoopCsms {
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
struct FastBackoff;

#[async_trait::async_trait]
impl Backoff for FastBackoff {
    async fn wait(&self, _seconds: u32) {
        tokio::time::sleep(StdDuration::from_millis(5)).await;
    }
}

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

/// Starts a local (no-CSMS) charger, the same way `start_local_charger` does (Authorization
/// against [`LocalAuthorizer`], no dial, no registration), and - **only** when `config` declares
/// `capabilities.smart_charging`, exactly the gate `connect_charger`'s `register_setup_blocks`
/// applies for a live CSMS - additionally registers the Smart Charging functional block against
/// [`NoopCsms`], which is what actually starts the composite-schedule projection loops. Returns
/// the runtime plus a live [`FakeChargePoint`] handle to tick (see `charger/running_charger.rs`'s
/// doc comment on why a handle has to be cloned out before the other clone's ownership moves into
/// the builder).
async fn start_charger(
    config: &ChargerConfig,
    clock: TestClock,
) -> (ChargePointRuntime<FakeChargePoint>, FakeChargePoint) {
    let hardware = FakeChargePoint::from_config(config);
    let handle = hardware.clone();

    let builder = ChargePointBuilder::start(hardware, TokioExecutor)
        .await
        .unwrap_or_else(|error: core::convert::Infallible| match error {});
    let builder = builder.authorization(&LocalAuthorizer, clock.clone()).await;
    let builder = if config.capabilities().smart_charging {
        builder
            .smart_charging(
                &NoopCsms,
                Arc::new(ChargingLimitProjection::new()),
                clock,
                FastBackoff,
            )
            .await
    } else {
        builder
    };

    (builder.build(), handle)
}

// ---------------------------------------------------------------------------------------------
// Driving a session and reading state back - the `RunningCharger` test pattern from
// `charger/running_charger.rs`, reproduced against a bare `ChargePointRuntime` since this file
// doesn't have a `RunningCharger` to hand (see this module's doc comment).
// ---------------------------------------------------------------------------------------------

fn connector_event(evse_id: usize, connector_id: usize, event: ConnectorEvent) -> ChargePointEvent {
    ChargePointEvent::Evse {
        evse_id,
        event: EvseEvent::Connector {
            connector_id,
            event,
        },
    }
}

/// Drives `runtime` through a full local session up to `Charging`: cable connected, locked
/// (automatic, via the real hardware round trip), an identifier presented, and authorized
/// (automatic, via [`LocalAuthorizer`]) - see `charger/running_charger.rs`'s `charge_locally` for
/// the original. Times out rather than hanging forever if a transition never lands.
async fn charge_locally(
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

/// Projects both `runtime`'s OCPP state and `hardware`'s hardware-only state (lock, contactor,
/// current limit - H7) onto a fresh [`ChargerState`], the same two-call sequence
/// `RunningCharger::apply_state` uses, reproduced here since this file has no `RunningCharger`.
fn read_state(
    runtime: &ChargePointRuntime<FakeChargePoint>,
    hardware: &FakeChargePoint,
    config: &ChargerConfig,
) -> ChargerState {
    let mut state = ChargerState::from_config(config.clone());
    apply_ocpp_state(&mut state, &runtime.state());
    apply_hardware_state(&mut state, hardware);
    state
}

/// Polls (via the state watch, never a `sleep`) until `ConnectorState::current_limit_ma` for
/// `evse_id`/`connector_id` reads `expected`, or panics after a timeout. The limit's path to
/// hardware crosses two async hops after any single event this test sends - the projection loop
/// noticing a state change and computing a limit, then the spawned hardware-command loop in
/// `FakeChargePoint::start` actually calling `Connector::set_current_limit` - so nothing about it
/// is synchronous with `runtime.send(..).await` returning, and polling is the correct tool per
/// `charger/running_charger.rs`'s own `wait_for`.
async fn wait_for_current_limit(
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
            let state = read_state(runtime, hardware, config);
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

    let unrestricted_clock = TestClock::new(epoch());
    let (unrestricted, unrestricted_hw) = start_charger(&cfg, unrestricted_clock.clone()).await;
    charge_locally(&unrestricted, 0, 0).await;

    let restricted_clock = TestClock::new(epoch());
    let (restricted, restricted_hw) = start_charger(&cfg, restricted_clock.clone()).await;
    charge_locally(&restricted, 0, 0).await;
    install_profile(
        &restricted,
        amp_profile(1, epoch(), None, vec![flat_period(8.0)]),
    )
    .await;
    wait_for_current_limit(&restricted, &restricted_hw, &cfg, 0, 0, Some(8_000)).await;

    unrestricted_clock.advance(StdDuration::from_secs(3_600));
    unrestricted_hw.tick(StdDuration::from_secs(3_600)).await;

    restricted_clock.advance(StdDuration::from_secs(3_600));
    restricted_hw.tick(StdDuration::from_secs(3_600)).await;

    let unrestricted_energy = read_state(&unrestricted, &unrestricted_hw, &cfg).evses[0]
        .metrics
        .energy_kwh;
    let restricted_energy = read_state(&restricted, &restricted_hw, &cfg).evses[0]
        .metrics
        .energy_kwh;

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
    let clock = TestClock::new(epoch());
    let (runtime, hardware) = start_charger(&cfg, clock.clone()).await;
    charge_locally(&runtime, 0, 0).await;

    install_profile(
        &runtime,
        amp_profile(1, epoch(), None, vec![flat_period(0.0)]),
    )
    .await;
    wait_for_current_limit(&runtime, &hardware, &cfg, 0, 0, Some(0)).await;

    clock.advance(StdDuration::from_secs(3_600));
    hardware.tick(StdDuration::from_secs(3_600)).await;

    let state = read_state(&runtime, &hardware, &cfg);
    assert_eq!(
        state.evses[0].metrics.energy_kwh, 0.0,
        "a Some(0) limit should halt accrual entirely"
    );
    assert_eq!(
        runtime.state().evses[0].connectors[0],
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
    let clock = TestClock::new(epoch());
    let (runtime, hardware) = start_charger(&cfg, clock.clone()).await;
    charge_locally(&runtime, 0, 0).await;

    install_profile(
        &runtime,
        amp_profile(1, epoch(), None, vec![flat_period(8.0)]),
    )
    .await;
    wait_for_current_limit(&runtime, &hardware, &cfg, 0, 0, Some(8_000)).await;

    clock.advance(StdDuration::from_secs(3_600));
    hardware.tick(StdDuration::from_secs(3_600)).await;
    let limited_energy = read_state(&runtime, &hardware, &cfg).evses[0]
        .metrics
        .energy_kwh;
    assert!(
        limited_energy > 0.0 && limited_energy < NOMINAL_HOURLY_KWH,
        "the limited hour should accrue less than the nominal rate, got {limited_energy}"
    );

    clear_all_profiles(&runtime).await;
    wait_for_current_limit(&runtime, &hardware, &cfg, 0, 0, None).await;

    clock.advance(StdDuration::from_secs(3_600));
    hardware.tick(StdDuration::from_secs(3_600)).await;
    let restored_energy = read_state(&runtime, &hardware, &cfg).evses[0]
        .metrics
        .energy_kwh;

    let second_hour = restored_energy - limited_energy;
    assert!(
        (second_hour - NOMINAL_HOURLY_KWH).abs() < 0.05,
        "the second hour, with the limit cleared, should accrue a full nominal hour \
         ({NOMINAL_HOURLY_KWH} kWh), got {second_hour}"
    );
}

/// A stepped schedule must change the *applied* limit exactly when simulated time crosses a
/// period boundary - not before, and not only once the test asks for the final value.
#[tokio::test]
async fn a_stepped_schedule_changes_the_applied_limit_at_the_period_boundary() {
    let cfg = config(true);
    let clock = TestClock::new(epoch());
    let (runtime, hardware) = start_charger(&cfg, clock.clone()).await;
    charge_locally(&runtime, 0, 0).await;

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
    wait_for_current_limit(&runtime, &hardware, &cfg, 0, 0, Some(8_000)).await;

    // Not yet at the boundary: still the first period's limit.
    clock.advance(StdDuration::from_secs(900));
    hardware.tick(StdDuration::from_secs(900)).await;
    let state = read_state(&runtime, &hardware, &cfg);
    assert_eq!(
        state.evses[0].connectors[0].current_limit_ma,
        Some(8_000),
        "short of the 1800s boundary, the limit should not have changed yet"
    );

    // Cross the boundary.
    clock.advance(StdDuration::from_secs(900));
    hardware.tick(StdDuration::from_secs(900)).await;
    wait_for_current_limit(&runtime, &hardware, &cfg, 0, 0, Some(16_000)).await;
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
    let clock = TestClock::new(epoch());
    let (runtime, hardware) = start_charger(&cfg, clock.clone()).await;
    charge_locally(&runtime, 0, 0).await;

    install_profile(
        &runtime,
        amp_profile(1, epoch(), None, vec![flat_period(8.0)]),
    )
    .await;

    clock.advance(StdDuration::from_secs(3_600));
    hardware.tick(StdDuration::from_secs(3_600)).await;

    let state = read_state(&runtime, &hardware, &cfg);
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
