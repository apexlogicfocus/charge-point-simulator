//! Proves (or fails to prove, honestly) the two capability-gated functional blocks
//! `docs/hardware-roadmap.md`'s H9 names: reservation and the local authorization list.
//!
//! Both blocks' CSMS-facing registrations already exist - `connect.rs`'s
//! `register_setup_blocks` has registered `reservation`/`reservation_status_updates` (gated on
//! `capabilities.reservation`) and `local_authorization_list` (gated on
//! `capabilities.local_auth_list`) since H2 reproduced `ocpp_charge_point::setup()`'s full
//! registration list. Neither had ever been driven end to end, which is what this file is for.
//!
//! Deliberately an **integration test**: everything below goes through
//! `charge_point_simulator_core`'s public API only, reached the same way a downstream consumer
//! of the crates.io crate would reach it (see `CLAUDE.md` on `core`'s public surface being a
//! supported one). In particular this file never touches `connect.rs`'s private
//! `register_setup_blocks`, and never dials a real CSMS - both would require driving
//! `connect_charger`, which negotiates a real WebSocket session (see `connect.rs`'s own
//! `#[ignore]`d `can_connect_to_the_local_dev_csms`, which needs a live dev CSMS to run at all).
//! Instead, every test here drives a [`start_local_charger`] session directly with
//! `ChargePointRuntime::send` - a real `ocpp_charge_point::ChargePointRuntime` and connector
//! state machine, just with no CSMS ever dialed.
//!
//! # What is, and isn't, provable this way
//!
//! - **Reservation's state-machine effect (points 1-2 of the H9 brief) is solidly provable.**
//!   `ocpp_charge_point::reservation::handle_reserve_now`/`handle_cancel_reservation` - what
//!   actually runs when a CSMS's `ReserveNow`/`CancelReservation` reaches a registered handler -
//!   do nothing more than send exactly the `ConnectorEvent::Reserved`/`ReservationCancelled`
//!   events this file sends directly. Driving them by hand is a faithful stand-in for "the CSMS
//!   reserved this connector", not a shortcut around it.
//! - **Whether `capabilities.reservation` actually gates that from happening (point 3) is
//!   NOT provable this way**, and the tests below say so with evidence rather than pretending
//!   otherwise. The connector state machine's own transition table
//!   (`ConnectorState::apply` in the vendored `ocpp-charge-point` source) takes
//!   `(Available, ConnectorEvent::Reserved(_)) -> Reserved` unconditionally - it has no
//!   `Capabilities` check of its own. The only place `capabilities.reservation` is ever
//!   consulted in this crate is `connect.rs`'s `if capabilities.reservation { ... }` around the
//!   `.reservation(csms)` registration - CSMS-registration-time, reachable only through
//!   `connect_charger` against a live CSMS. `start_local_charger` never calls
//!   `register_setup_blocks` at all (it registers only `authorization()`), so no capability of
//!   any kind is ever consulted on the path this file can drive. What this file *can* and does
//!   prove is that the low-level mechanism itself carries no such gate, which is exactly why the
//!   CSMS-registration gate is where all of the enforcement necessarily lives.
//! - **The local authorization list's rejection path (half of point 4) is NOT observable**,
//!   for a related but distinct reason, and the test below proves the *un*-observability
//!   directly rather than asserting around it. `start_local_charger` (H3b) registers a
//!   `LocalAuthorizer` whose `authorize()` is `Infallible` and always returns
//!   `Ok(AuthorizationStatus::Accepted)`. The only place `ChargePointState.local_authorization_list`
//!   is ever read is `ocpp_charge_point::authorization::offline_decision`, called exclusively
//!   from `plain_decision`'s `Err(_)` arm - i.e. only once `Authorizer::authorize` itself fails.
//!   Since `LocalAuthorizer` can't fail, that arm is dead code on every local charger, list
//!   contents included. A locally listed identifier is accepted - trivially, the same as any
//!   identifier - and, as
//!   [`local_auth_list_rejection_is_not_observable_in_local_mode`] demonstrates, so is one the
//!   list explicitly rejects.

use std::time::Duration as StdDuration;

use charge_point_simulator_core::charger::{
    CapabilitiesConfig, ChargerConfig, ChargerState, ConnectorStatus, EvseConfig, OcppVersion,
    RunningCharger, start_local_charger,
};
use ocpp_charge_point::state::{
    AuthorizationStatus, ChargePointEvent, ConnectorEvent, ConnectorState as OcppConnectorState,
    EvseEvent, IdToken, IdTokenKind, LocalListEntry, Reservation, ReservationId,
};
use ocpp_charge_point::sync::WatchReceiver;

fn config(capabilities: CapabilitiesConfig) -> ChargerConfig {
    ChargerConfig {
        id: "H9-TEST".into(),
        ocpp_version: OcppVersion::V21,
        evses: vec![EvseConfig {
            id: 1,
            connectors: 1,
        }],
        has_display: false,
        capabilities,
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

fn test_id_token(value: &str) -> IdToken {
    IdToken {
        value: value.into(),
        kind: IdTokenKind::ISO14443,
    }
}

/// Waits (with a timeout, so a regression fails the test instead of hanging the suite) until
/// `evse_id`/`connector_id` reaches `target` - the same pattern
/// `charger/running_charger.rs`'s own tests use, reproduced here since that module's test-only
/// helpers aren't part of the public API this file is restricted to.
async fn wait_for(
    states: &mut WatchReceiver<ocpp_charge_point::state::ChargePointState>,
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

/// Reads `evse_id`/`connector_id`'s projected status via [`RunningCharger::apply_state`] - the
/// same public path a frontend (the TUI, or any other downstream consumer) uses to read a
/// charger's state, rather than reaching into `ChargePointState` directly.
fn projected_connector(
    charger: &RunningCharger,
    config: &ChargerConfig,
    evse_id: usize,
    connector_id: usize,
) -> charge_point_simulator_core::charger::ConnectorState {
    let mut state = ChargerState::from_config(config.clone());
    charger.apply_state(&mut state);
    state.evses[evse_id].connectors[connector_id].clone()
}

// --- Reservation: points 1-2 -----------------------------------------------------------------

/// H9 point 1: a charger declaring `reservation`, reserved via the same
/// `ConnectorEvent::Reserved` a CSMS's `ReserveNow` would produce, surfaces as
/// `ConnectorStatus::Reserved` through `apply_state` with no vehicle attached -
/// `charger/ocpp_bridge.rs` deliberately does not synthesize a placeholder vehicle for a
/// reservation, since nothing is actually plugged in.
#[tokio::test]
async fn a_charger_declaring_reservation_can_have_a_connector_reserved_with_no_vehicle_attached() {
    let config = config(CapabilitiesConfig {
        reservation: true,
        ..Default::default()
    });
    let charger = start_local_charger(&config).await;
    let mut states = charger.subscribe();

    // Before anything happens, the connector reads its construction-time default.
    assert_eq!(
        projected_connector(&charger, &config, 0, 0).status,
        ConnectorStatus::Available
    );

    charger
        .send(connector_event(
            0,
            0,
            ConnectorEvent::Reserved(Reservation {
                id: ReservationId(1),
                id_token: test_id_token("RESERVED-FOR-ME"),
                expires_at: None,
            }),
        ))
        .await
        .unwrap();
    wait_for(&mut states, 0, 0, OcppConnectorState::Reserved).await;

    let connector = projected_connector(&charger, &config, 0, 0);
    assert_eq!(connector.status, ConnectorStatus::Reserved);
    assert!(
        connector.vehicle.is_none(),
        "a reservation holds a connector for someone who hasn't arrived yet - nothing should be \
         plugged in"
    );
}

/// H9 point 2: cancelling a reservation returns the connector to `Available`.
#[tokio::test]
async fn cancelling_a_reservation_returns_the_connector_to_available() {
    let config = config(CapabilitiesConfig {
        reservation: true,
        ..Default::default()
    });
    let charger = start_local_charger(&config).await;
    let mut states = charger.subscribe();

    charger
        .send(connector_event(
            0,
            0,
            ConnectorEvent::Reserved(Reservation {
                id: ReservationId(1),
                id_token: test_id_token("RESERVED-FOR-ME"),
                expires_at: None,
            }),
        ))
        .await
        .unwrap();
    wait_for(&mut states, 0, 0, OcppConnectorState::Reserved).await;

    charger
        .send(connector_event(0, 0, ConnectorEvent::ReservationCancelled))
        .await
        .unwrap();
    wait_for(&mut states, 0, 0, OcppConnectorState::Available).await;

    let connector = projected_connector(&charger, &config, 0, 0);
    assert_eq!(connector.status, ConnectorStatus::Available);
    assert!(connector.vehicle.is_none());
}

/// H9 point 3, and the limit of what this file can prove about it. `capabilities.reservation`
/// is read in exactly one place in this crate - `connect.rs`'s `register_setup_blocks`, gating
/// whether `.reservation(csms)` (and so the CSMS's `ReserveNow`/`CancelReservation` handlers)
/// ever gets registered. `start_local_charger` never calls `register_setup_blocks` at all, and
/// the connector state machine's own `(Available, Reserved(_)) -> Reserved` transition
/// (`ocpp-charge-point`'s `state::connector_state`) carries no capability check of its own.
///
/// So driving the *identical* event at a charger that declares `reservation` and one that
/// doesn't produces the *identical* result - proven directly below - which demonstrates the gate
/// this crate relies on lives entirely in CSMS registration, not in the mechanism this test can
/// reach. Proving the registration gate itself needs a live (or faked) CSMS connection through
/// `connect_charger`, which is outside what "no CSMS at all" can exercise; see this file's module
/// doc comment.
#[tokio::test]
async fn reservation_capability_has_no_effect_on_the_connector_state_machine_itself() {
    let declares_it = config(CapabilitiesConfig {
        reservation: true,
        ..Default::default()
    });
    let does_not_declare_it = config(CapabilitiesConfig {
        reservation: false,
        ..Default::default()
    });

    for config in [declares_it, does_not_declare_it] {
        let charger = start_local_charger(&config).await;
        let mut states = charger.subscribe();

        charger
            .send(connector_event(
                0,
                0,
                ConnectorEvent::Reserved(Reservation {
                    id: ReservationId(1),
                    id_token: test_id_token("RESERVED-FOR-ME"),
                    expires_at: None,
                }),
            ))
            .await
            .unwrap();
        wait_for(&mut states, 0, 0, OcppConnectorState::Reserved).await;

        assert_eq!(
            projected_connector(&charger, &config, 0, 0).status,
            ConnectorStatus::Reserved,
            "declaring capabilities.reservation = {} should not change whether a directly-sent \
             Reserved event takes effect - the state machine itself has no such gate",
            config.capabilities.reservation
        );
    }
}

// --- Local authorization list: point 4 ---------------------------------------------------------

/// H9 point 4, positive half: a charger declaring `local_auth_list`, with an identifier seeded
/// into its local list via `ChargePointEvent::LocalListUpdated` (exactly what
/// `ocpp_charge_point::local_authorization_list::handle_send_local_list` sends once it resolves
/// a CSMS's `SendLocalList` - driven directly here since there is no CSMS), authorizes charging
/// for that identifier with no CSMS involved at all.
#[tokio::test]
async fn a_charger_declaring_local_auth_list_authorizes_a_locally_listed_identifier() {
    let config = config(CapabilitiesConfig {
        local_auth_list: true,
        ..Default::default()
    });
    let charger = start_local_charger(&config).await;
    let mut states = charger.subscribe();

    charger
        .send(ChargePointEvent::LocalListUpdated {
            version: 1,
            entries: vec![LocalListEntry {
                id_token: test_id_token("LISTED-TAG"),
                status: AuthorizationStatus::Accepted,
            }],
        })
        .await
        .unwrap();

    charger
        .send(connector_event(0, 0, ConnectorEvent::CableConnected))
        .await
        .unwrap();
    wait_for(&mut states, 0, 0, OcppConnectorState::Locked).await;

    charger
        .send(connector_event(
            0,
            0,
            ConnectorEvent::IdTokenPresented(test_id_token("LISTED-TAG")),
        ))
        .await
        .unwrap();
    wait_for(&mut states, 0, 0, OcppConnectorState::Charging).await;

    assert_eq!(
        projected_connector(&charger, &config, 0, 0).status,
        ConnectorStatus::Charging
    );
}

/// H9 point 4, negative half - and the finding this test exists to surface rather than paper
/// over: **a local charger's local authorization list can never actually reject anything.**
///
/// `start_local_charger` (H3b) registers `LocalAuthorizer` as the Authorization block's
/// `Authorizer`, whose `authorize()` returns `Result<AuthorizationStatus, Infallible>` and always
/// answers `Ok(Accepted)`. `ocpp_charge_point::authorization::plain_decision` only ever consults
/// the local authorization list (via `offline_decision`) from its `Err(_)` arm - the fallback
/// path for when asking the CSMS itself failed. An `Authorizer` that cannot fail can never reach
/// that arm, so `ChargePointState.local_authorization_list` is *stored* (this crate can seed and
/// read it back, as the test above shows) but never *consulted* by a local charger, regardless of
/// what it contains.
///
/// Proven directly: the list below explicitly rejects this identifier, and charging starts
/// anyway. If the local authorization list's rejection path worked from the outside, this test
/// would hang waiting for `Locked` (denial leaves the connector locked, never `Charging` - see
/// `ocpp_charge_point::authorization`'s own
/// `a_rejected_decision_leaves_the_connector_locked` test) and time out instead of passing.
#[tokio::test]
async fn local_auth_list_rejection_is_not_observable_in_local_mode() {
    let config = config(CapabilitiesConfig {
        local_auth_list: true,
        ..Default::default()
    });
    let charger = start_local_charger(&config).await;
    let mut states = charger.subscribe();

    charger
        .send(ChargePointEvent::LocalListUpdated {
            version: 1,
            entries: vec![LocalListEntry {
                id_token: test_id_token("BLOCKED-TAG"),
                status: AuthorizationStatus::Rejected,
            }],
        })
        .await
        .unwrap();

    charger
        .send(connector_event(0, 0, ConnectorEvent::CableConnected))
        .await
        .unwrap();
    wait_for(&mut states, 0, 0, OcppConnectorState::Locked).await;

    charger
        .send(connector_event(
            0,
            0,
            ConnectorEvent::IdTokenPresented(test_id_token("BLOCKED-TAG")),
        ))
        .await
        .unwrap();
    // This is the finding, not an oversight: a real local authorization list would leave the
    // connector `Locked` here. It reaches `Charging` instead, because `LocalAuthorizer` accepted
    // before the list was ever read.
    wait_for(&mut states, 0, 0, OcppConnectorState::Charging).await;

    assert_eq!(
        projected_connector(&charger, &config, 0, 0).status,
        ConnectorStatus::Charging,
        "an identifier the local authorization list explicitly rejects still started charging - \
         see this test's doc comment for why that is a finding about LocalAuthorizer, not a bug \
         in this test"
    );
}
