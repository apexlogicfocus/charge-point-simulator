//! Bridges a live, connected OCPP 2.1 charger's real protocol state (from `ocpp-charge-point`)
//! with the simulator's own coarse [`ChargerState`]/[`Command`] model. Pure mapping/decision
//! logic only - the thread and channel plumbing that carries values across this boundary lives
//! in the TUI, since it's integration glue rather than testable state logic.

use ocpp_charge_point::hardware::{ChargePoint, Evse};
use ocpp_charge_point::state::{
    ChargePointEvent, ChargePointState, ConnectorEvent, ConnectorState as OcppConnectorState,
    EvseEvent, IdToken, IdTokenKind, MeterSample, RegistrationStatus, StopReason,
};

use super::command::Command;
use super::hardware::FakeChargePoint;
use super::state::{
    ChargerState, ConnectionStatus, ConnectorStatus, EvseMetrics, SimulationMode, Vehicle,
};

/// Maps a connector's real OCPP protocol state (the full lock/authorize/charge lifecycle) down
/// to the simulator's coarse display status.
pub fn map_connector_status(state: OcppConnectorState) -> ConnectorStatus {
    match state {
        OcppConnectorState::Available => ConnectorStatus::Available,
        OcppConnectorState::Connected
        | OcppConnectorState::Locked
        | OcppConnectorState::Authorizing
        | OcppConnectorState::Stopping
        | OcppConnectorState::Finishing
        | OcppConnectorState::Unlocking => ConnectorStatus::Occupied,
        OcppConnectorState::Starting | OcppConnectorState::Charging => ConnectorStatus::Charging,
        // Suspended by either side is still an active session with a cable in it - the simulator's
        // coarse status has no "suspended", and "occupied" is closer than "charging" since no
        // energy is flowing.
        OcppConnectorState::SuspendedEv | OcppConnectorState::SuspendedEvse => {
            ConnectorStatus::Occupied
        }
        OcppConnectorState::Reserved => ConnectorStatus::Reserved,
        OcppConnectorState::Unavailable => ConnectorStatus::Unavailable,
        OcppConnectorState::Faulted | OcppConnectorState::FaultedSafe => ConnectorStatus::Faulted,
    }
}

/// Maps the CSMS's most recent BootNotification decision to the simulator's coarse CSMS-link
/// status. `None` (no response yet) reads as still booting; `Pending`/`Rejected` as
/// reconnecting, since `ChargePointRuntime::register_until_accepted` is still retrying.
pub fn map_connection_status(registration: Option<RegistrationStatus>) -> ConnectionStatus {
    match registration {
        None => ConnectionStatus::Booting,
        Some(RegistrationStatus::Accepted) => ConnectionStatus::Connected,
        Some(RegistrationStatus::Pending) | Some(RegistrationStatus::Rejected) => {
            ConnectionStatus::Reconnecting
        }
    }
}

/// Overwrites `charger`'s connection/connector status and per-EVSE meter reading from a live
/// `ChargePointState` snapshot, keyed positionally (charger and OCPP EVSEs/connectors are built
/// from the same config, in the same order). The single path into `ChargerState` from a real
/// `ChargePointState`, for a local (unconnected) charger and a live-CSMS one alike
/// (`docs/hardware-roadmap.md`'s H3b) - `ChargerState`/`EvseState::tick` no longer simulate
/// anything electrical or drive `connection_status` themselves.
///
/// Synthesizes a placeholder vehicle the first time a connector reports a plugged-in state, and
/// clears it once free again; a vehicle already known locally (e.g. named via a "Plug in
/// vehicle" parameter prompt) is left alone rather than overwritten every snapshot.
pub fn apply_ocpp_state(charger: &mut ChargerState, ocpp_state: &ChargePointState) {
    charger.connection_status = match &charger.mode {
        // No CSMS was ever dialed, so `ocpp_state.registration` stays `None` forever - reading
        // that (the way `map_connection_status` does for a live connection) as "still booting"
        // would report booting forever, which is the bug this branch exists to avoid (H3b). A
        // charger with no CSMS in the picture at all is honestly `Offline`.
        SimulationMode::Local => ConnectionStatus::Offline,
        SimulationMode::LiveCsms { .. } => map_connection_status(ocpp_state.registration),
    };

    for (evse, ocpp_evse) in charger.evses.iter_mut().zip(ocpp_state.evses.iter()) {
        evse.metrics = evse_metrics_from_samples(&ocpp_evse.latest_meter_samples);

        for (connector, &ocpp_connector) in
            evse.connectors.iter_mut().zip(ocpp_evse.connectors.iter())
        {
            connector.status = map_connector_status(ocpp_connector);
            if matches!(
                ocpp_connector,
                OcppConnectorState::Available
                    | OcppConnectorState::Unavailable
                    | OcppConnectorState::Reserved
            ) {
                connector.vehicle = None;
            } else if connector.vehicle.is_none() {
                connector.vehicle = Some(Vehicle {
                    id: format!("EV-E{}C{}", evse.id, connector.id),
                    state_of_charge: None,
                });
            }
        }
    }
}

/// Overwrites `charger`'s per-connector lock, contactor and applied current-limit state from a
/// running charger's fake hardware handle, keyed positionally exactly like [`apply_ocpp_state`] -
/// `charger` and `hardware`'s EVSEs/connectors are built from the same
/// [`super::config::ChargerConfig`] in the same order, so index (not
/// [`super::hardware::FakeConnector`]'s own `evse_id`/`connector_id`, which carry the config's
/// possibly non-1-based or non-contiguous numbering - see [`FakeChargePoint::tick`]'s doc
/// comment) is the addressing scheme.
///
/// A separate function from `apply_ocpp_state` rather than folded into it, because these three
/// fields have no `ChargePointState` counterpart to read at all - lock, contactor and current
/// limit live only on [`super::hardware::FakeConnector`], in the hardware layer
/// (`docs/hardware-roadmap.md`'s H7). [`super::running_charger::RunningCharger::apply_state`] is
/// the one place both an OCPP snapshot and a hardware handle are reachable together, and calls
/// this alongside `apply_ocpp_state` - callers who only care about one signal (e.g. these tests)
/// can reach for either projection on its own.
pub fn apply_hardware_state(charger: &mut ChargerState, hardware: &FakeChargePoint) {
    apply_hardware_snapshot(charger, &hardware_snapshot(hardware));
}

/// Everything about one connector that lives *only* in the hardware layer, with no
/// `ChargePointState` counterpart to read instead: the H7 lock/contactor/current-limit trio and
/// H14's power direction plus exported-energy register.
///
/// A plain, owned, `Copy` value so it can cross a thread boundary, which is the whole reason this
/// type exists. [`apply_hardware_state`] is enough for a caller holding a
/// [`FakeChargePoint`] on the same thread it renders from, but
/// [`super::running_charger::RunningCharger`] keeps its hardware handle private and is not `Send`,
/// so a frontend that drives the runtime on its own thread - the TUI, and any downstream consumer
/// of the published crate shaped the same way - can never call `apply_hardware_state` itself. It
/// forwards a snapshot instead (see [`super::running_charger::RunningCharger::hardware_snapshot`])
/// and applies it with [`apply_hardware_snapshot`], which is the same projection
/// `apply_hardware_state` performs, split in two at the point where the value stops needing the
/// hardware.
///
/// Every field is the source for the [`ConnectorState`] field of the same name, whose doc comment
/// is where each one is actually explained.
///
/// [`ConnectorState`]: super::state::ConnectorState
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub struct ConnectorHardwareSnapshot {
    pub locked: bool,
    pub contactor_closed: bool,
    pub current_limit_ma: Option<u32>,
    pub discharging: bool,
    pub exported_energy_wh: i64,
}

/// Reads every connector's hardware-only state off `hardware`, outer `Vec` per EVSE and inner per
/// connector, in the hardware's own array order - the positional addressing
/// [`apply_hardware_state`]'s doc comment explains, preserved here so a snapshot lines up with a
/// [`ChargerState`] built from the same config exactly as the hardware handle itself does.
pub fn hardware_snapshot(hardware: &FakeChargePoint) -> Vec<Vec<ConnectorHardwareSnapshot>> {
    hardware
        .evses()
        .iter()
        .map(|hw_evse| {
            hw_evse
                .connectors()
                .iter()
                .map(|hw_connector| ConnectorHardwareSnapshot {
                    locked: hw_connector.is_locked(),
                    contactor_closed: hw_connector.is_contactor_closed(),
                    current_limit_ma: hw_connector.current_limit_ma(),
                    discharging: hw_connector.is_discharging(),
                    exported_energy_wh: hw_connector.exported_energy_wh(),
                })
                .collect()
        })
        .collect()
}

/// Overwrites `charger`'s hardware-only per-connector state from `snapshot`, which
/// [`hardware_snapshot`] produced - the half of [`apply_hardware_state`] that no longer needs the
/// hardware handle, so it can run on whichever thread renders (see [`ConnectorHardwareSnapshot`]).
///
/// Positional, and tolerant of a length mismatch in either direction: a connector with no entry
/// keeps whatever it had rather than being reset, exactly as `apply_hardware_state`'s `zip`
/// already behaved. In practice both sides come from the same [`super::config::ChargerConfig`], so
/// a mismatch means a snapshot from a *different* charger arrived - and writing a foreign
/// charger's lock state onto this one's connectors would be worse than writing nothing.
pub fn apply_hardware_snapshot(
    charger: &mut ChargerState,
    snapshot: &[Vec<ConnectorHardwareSnapshot>],
) {
    for (evse, hw_evse) in charger.evses.iter_mut().zip(snapshot.iter()) {
        for (connector, hw_connector) in evse.connectors.iter_mut().zip(hw_evse.iter()) {
            connector.locked = hw_connector.locked;
            connector.contactor_closed = hw_connector.contactor_closed;
            connector.current_limit_ma = hw_connector.current_limit_ma;
            connector.discharging = hw_connector.discharging;
            connector.exported_energy_wh = hw_connector.exported_energy_wh;
        }
    }
}

/// Aggregates one EVSE's per-connector meter samples into the coarser [`EvseMetrics`] the
/// dashboard renders - this simulator has no EVSE-level meter of its own any more (H3b moved the
/// only meter down to `SimulatedMeter`, one per connector), so an EVSE's reading is always the
/// sum of what its connectors report, the same way multiple charging connectors on one EVSE used
/// to simply add up in the old `EvseState::tick` accumulator.
///
/// A connector that hasn't reported a sample yet (`None` - nothing has ticked since this charger
/// started) contributes zero rather than being skipped or treated as "unknown": that is the same
/// "measured zero, not unmeasurable" stance `SimulatedMeter` itself takes once it *has* ticked
/// (`Some(0)`, never `None`, for an idle connector - see `docs/hardware-roadmap.md`'s H3b), and a
/// connector nothing has ticked yet is idle in exactly the same sense.
fn evse_metrics_from_samples(samples: &[Option<MeterSample>]) -> EvseMetrics {
    let mut power_w = 0i64;
    let mut current_ma = 0i64;
    let mut energy_wh = 0i64;
    for sample in samples.iter().flatten() {
        power_w += sample.power_w.unwrap_or(0);
        current_ma += sample.current_ma.unwrap_or(0);
        energy_wh += sample.energy_wh;
    }
    EvseMetrics {
        power_kw: power_w as f64 / 1000.0,
        current_a: current_ma as f64 / 1000.0,
        energy_kwh: energy_wh as f64 / 1000.0,
    }
}

/// The single `ConnectorEvent` that dispatching `command` against a connector currently in the
/// real OCPP `state` would produce, or `None` if `command` isn't valid there. Mirrors
/// [`Command::is_available`]/[`Command::apply`]'s local simulation, but against the protocol's
/// actual, more fine-grained connector lifecycle - e.g. presenting an RFID card is only
/// meaningful once the connector has reached `Locked`, not merely `Connected`.
pub fn command_to_connector_event(
    command: Command,
    state: OcppConnectorState,
    input: &str,
) -> Option<ConnectorEvent> {
    let input = input.trim();
    match (command, state) {
        (Command::PlugInVehicle, OcppConnectorState::Available) => {
            Some(ConnectorEvent::CableConnected)
        }
        (Command::PresentRfid, OcppConnectorState::Locked) => {
            let value = if input.is_empty() {
                "UNKNOWN".to_string()
            } else {
                input.to_string()
            };
            Some(ConnectorEvent::IdTokenPresented(IdToken {
                value,
                kind: IdTokenKind::ISO14443,
            }))
        }
        (Command::UnplugVehicle, OcppConnectorState::Charging) => {
            Some(ConnectorEvent::ChargingStopped(StopReason::Local))
        }
        (Command::UnplugVehicle, OcppConnectorState::Connected) => {
            Some(ConnectorEvent::CableDisconnected)
        }
        (Command::ReportFault, state)
            if !matches!(
                state,
                OcppConnectorState::Faulted | OcppConnectorState::FaultedSafe
            ) =>
        {
            Some(ConnectorEvent::FaultDetected)
        }
        (Command::ClearFault, OcppConnectorState::Faulted | OcppConnectorState::FaultedSafe) => {
            Some(ConnectorEvent::FaultCleared)
        }
        _ => None,
    }
}

/// Builds the full event to send to the runtime for dispatching `command` against one specific
/// connector (`evse_id`/`connector_id`) in `ocpp_state`, mirroring [`Command::apply_to`]'s
/// single-connector semantics against the protocol's actual, more fine-grained connector
/// lifecycle. Returns `None` if `evse_id`/`connector_id` doesn't exist or that connector isn't
/// eligible for `command` yet - e.g. the connector hasn't finished a hardware handshake the
/// coarse local status already shows as done. A real connector reports "occupied" for
/// `Connected` through `Unlocking` alike, so a command needing a more specific fine-grained
/// state can briefly appear available before it actually is.
pub fn build_ocpp_event_for_connector(
    ocpp_state: &ChargePointState,
    evse_id: usize,
    connector_id: usize,
    command: Command,
    input: &str,
) -> Option<ChargePointEvent> {
    let evse = ocpp_state.evses.get(evse_id)?;
    let &state = evse.connectors.get(connector_id)?;
    let event = command_to_connector_event(command, state, input)?;

    Some(ChargePointEvent::Evse {
        evse_id,
        event: EvseEvent::Connector {
            connector_id,
            event,
        },
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::charger::config::{CapabilitiesConfig, ChargerConfig, EvseConfig, OcppVersion};
    use crate::charger::state::{ChargerState, ConnectorStatus, SimulationMode};
    use ocpp_charge_point::state::LifecycleState;

    fn charger_state() -> ChargerState {
        ChargerState::from_config(ChargerConfig {
            id: "CP001".into(),
            ocpp_version: OcppVersion::V21,
            evses: vec![EvseConfig {
                id: 1,
                connectors: 2,
            }],
            has_display: false,
            capabilities: CapabilitiesConfig::default(),
        })
    }

    fn ocpp_state_with(connectors: Vec<OcppConnectorState>) -> ChargePointState {
        // Built through the crate's own constructor rather than a struct literal: `ChargePointState`
        // has grown a field per functional block, and only these three matter to this mapping.
        let mut state = ChargePointState::new([connectors.len()]);
        state.lifecycle = LifecycleState::Available;
        state.registration = Some(RegistrationStatus::Accepted);
        state.evses[0].connectors = connectors;
        state
    }

    #[test]
    fn maps_every_connector_state_to_a_coarse_status() {
        assert_eq!(
            map_connector_status(OcppConnectorState::Available),
            ConnectorStatus::Available
        );
        assert_eq!(
            map_connector_status(OcppConnectorState::Connected),
            ConnectorStatus::Occupied
        );
        assert_eq!(
            map_connector_status(OcppConnectorState::Locked),
            ConnectorStatus::Occupied
        );
        assert_eq!(
            map_connector_status(OcppConnectorState::Authorizing),
            ConnectorStatus::Occupied
        );
        assert_eq!(
            map_connector_status(OcppConnectorState::Starting),
            ConnectorStatus::Charging
        );
        assert_eq!(
            map_connector_status(OcppConnectorState::Charging),
            ConnectorStatus::Charging
        );
        assert_eq!(
            map_connector_status(OcppConnectorState::Stopping),
            ConnectorStatus::Occupied
        );
        assert_eq!(
            map_connector_status(OcppConnectorState::Finishing),
            ConnectorStatus::Occupied
        );
        assert_eq!(
            map_connector_status(OcppConnectorState::Unlocking),
            ConnectorStatus::Occupied
        );
        assert_eq!(
            map_connector_status(OcppConnectorState::SuspendedEv),
            ConnectorStatus::Occupied
        );
        assert_eq!(
            map_connector_status(OcppConnectorState::SuspendedEvse),
            ConnectorStatus::Occupied
        );
        assert_eq!(
            map_connector_status(OcppConnectorState::Reserved),
            ConnectorStatus::Reserved
        );
        assert_eq!(
            map_connector_status(OcppConnectorState::Unavailable),
            ConnectorStatus::Unavailable
        );
        assert_eq!(
            map_connector_status(OcppConnectorState::Faulted),
            ConnectorStatus::Faulted
        );
        assert_eq!(
            map_connector_status(OcppConnectorState::FaultedSafe),
            ConnectorStatus::Faulted
        );
    }

    #[test]
    fn maps_registration_outcomes_to_connection_status() {
        assert_eq!(map_connection_status(None), ConnectionStatus::Booting);
        assert_eq!(
            map_connection_status(Some(RegistrationStatus::Accepted)),
            ConnectionStatus::Connected
        );
        assert_eq!(
            map_connection_status(Some(RegistrationStatus::Pending)),
            ConnectionStatus::Reconnecting
        );
        assert_eq!(
            map_connection_status(Some(RegistrationStatus::Rejected)),
            ConnectionStatus::Reconnecting
        );
    }

    #[test]
    fn apply_ocpp_state_never_touches_mode_or_uptime() {
        let mut charger = charger_state();
        charger.mode = SimulationMode::LiveCsms {
            url: "ws://csms.example/CP001".into(),
        };
        charger.uptime = std::time::Duration::from_secs(42);
        let ocpp = ocpp_state_with(vec![
            OcppConnectorState::Locked,
            OcppConnectorState::Available,
        ]);

        apply_ocpp_state(&mut charger, &ocpp);

        assert_eq!(
            charger.mode,
            SimulationMode::LiveCsms {
                url: "ws://csms.example/CP001".into()
            }
        );
        assert_eq!(charger.uptime, std::time::Duration::from_secs(42));
    }

    #[test]
    fn a_live_csms_chargers_status_comes_from_the_bridge_never_from_tick() {
        let mut charger = charger_state();
        charger.mode = SimulationMode::LiveCsms {
            url: "ws://csms.example/CP001".into(),
        };

        // Ticking alone, however long, must never move a LiveCsms charger off Booting.
        charger.tick(std::time::Duration::from_secs(3600));
        assert_eq!(charger.connection_status, ConnectionStatus::Booting);

        // Only the bridge, mirroring the real CSMS registration outcome, may advance it.
        let ocpp = ocpp_state_with(vec![
            OcppConnectorState::Available,
            OcppConnectorState::Available,
        ]);
        apply_ocpp_state(&mut charger, &ocpp);
        assert_eq!(charger.connection_status, ConnectionStatus::Connected);
    }

    #[test]
    fn apply_ocpp_state_updates_connection_and_connector_status() {
        let mut charger = charger_state();
        charger.mode = SimulationMode::LiveCsms {
            url: "ws://csms.example/CP001".into(),
        };
        let ocpp = ocpp_state_with(vec![
            OcppConnectorState::Locked,
            OcppConnectorState::Available,
        ]);

        apply_ocpp_state(&mut charger, &ocpp);

        assert_eq!(charger.connection_status, ConnectionStatus::Connected);
        assert_eq!(
            charger.evses[0].connectors[0].status,
            ConnectorStatus::Occupied
        );
        assert_eq!(
            charger.evses[0].connectors[1].status,
            ConnectorStatus::Available
        );
    }

    /// H3b: with no CSMS at all, `ChargePointState::registration` never becomes `Some(..)` - a
    /// `Local` charger must read `Offline`, not the endlessly-retried `Booting`/`Reconnecting` an
    /// unanswered *real* CSMS registration would mean. This holds even if the snapshot happens to
    /// carry a `registration` value (it never will in practice for a local runtime, but the
    /// mapping must not depend on that not happening).
    #[test]
    fn a_local_chargers_connection_status_is_always_offline_regardless_of_registration() {
        let mut charger = charger_state();
        assert_eq!(charger.mode, SimulationMode::Local);
        let ocpp = ocpp_state_with(vec![
            OcppConnectorState::Available,
            OcppConnectorState::Available,
        ]);

        apply_ocpp_state(&mut charger, &ocpp);

        assert_eq!(charger.connection_status, ConnectionStatus::Offline);
    }

    #[test]
    fn apply_ocpp_state_populates_evse_metrics_from_a_connectors_meter_sample() {
        let mut charger = charger_state();
        let mut ocpp = ocpp_state_with(vec![
            OcppConnectorState::Charging,
            OcppConnectorState::Available,
        ]);
        ocpp.evses[0].latest_meter_samples[0] = Some(MeterSample {
            energy_wh: 1_500,
            power_w: Some(7_400),
            current_ma: Some(32_174),
            voltage_v: Some(230),
            soc_percent: None,
        });

        apply_ocpp_state(&mut charger, &ocpp);

        let metrics = charger.evses[0].metrics;
        assert!((metrics.energy_kwh - 1.5).abs() < 1e-9);
        assert!((metrics.power_kw - 7.4).abs() < 1e-9);
        assert!((metrics.current_a - 32.174).abs() < 1e-9);
    }

    #[test]
    fn apply_ocpp_state_sums_meter_samples_across_every_connector_on_an_evse() {
        let mut charger = charger_state();
        let mut ocpp = ocpp_state_with(vec![
            OcppConnectorState::Charging,
            OcppConnectorState::Charging,
        ]);
        ocpp.evses[0].latest_meter_samples[0] = Some(MeterSample {
            energy_wh: 1_000,
            power_w: Some(7_400),
            current_ma: Some(32_000),
            voltage_v: Some(230),
            soc_percent: None,
        });
        ocpp.evses[0].latest_meter_samples[1] = Some(MeterSample {
            energy_wh: 500,
            power_w: Some(3_700),
            current_ma: Some(16_000),
            voltage_v: Some(230),
            soc_percent: None,
        });

        apply_ocpp_state(&mut charger, &ocpp);

        let metrics = charger.evses[0].metrics;
        assert!((metrics.energy_kwh - 1.5).abs() < 1e-9);
        assert!((metrics.power_kw - 11.1).abs() < 1e-9);
        assert!((metrics.current_a - 48.0).abs() < 1e-9);
    }

    /// A connector nothing has ticked yet reports no sample at all (`None`, distinct from the
    /// hardware's own `Some(0)` for an idle-but-ticked connector - see
    /// `docs/hardware-roadmap.md`'s H3b). It must contribute zero, not be treated as unknown or
    /// panic the aggregation, and must not suppress a sibling connector's real reading.
    #[test]
    fn apply_ocpp_state_reads_a_connector_with_no_sample_yet_as_zero_not_unknown() {
        let mut charger = charger_state();
        let mut ocpp = ocpp_state_with(vec![
            OcppConnectorState::Available,
            OcppConnectorState::Charging,
        ]);
        ocpp.evses[0].latest_meter_samples[1] = Some(MeterSample {
            energy_wh: 500,
            power_w: Some(3_700),
            current_ma: Some(16_000),
            voltage_v: Some(230),
            soc_percent: None,
        });

        apply_ocpp_state(&mut charger, &ocpp);

        let metrics = charger.evses[0].metrics;
        assert!((metrics.energy_kwh - 0.5).abs() < 1e-9);
        assert!((metrics.power_kw - 3.7).abs() < 1e-9);
    }

    #[test]
    fn apply_ocpp_state_synthesizes_a_vehicle_the_first_time_a_connector_becomes_occupied() {
        let mut charger = charger_state();
        let ocpp = ocpp_state_with(vec![
            OcppConnectorState::Connected,
            OcppConnectorState::Available,
        ]);

        apply_ocpp_state(&mut charger, &ocpp);

        assert!(charger.evses[0].connectors[0].vehicle.is_some());
        assert!(charger.evses[0].connectors[1].vehicle.is_none());
    }

    #[test]
    fn apply_ocpp_state_leaves_an_already_known_vehicle_untouched() {
        let mut charger = charger_state();
        charger.evses[0].connectors[0].vehicle = Some(Vehicle {
            id: "MY-EV".into(),
            state_of_charge: Some(55.0),
        });
        let ocpp = ocpp_state_with(vec![
            OcppConnectorState::Charging,
            OcppConnectorState::Available,
        ]);

        apply_ocpp_state(&mut charger, &ocpp);

        let vehicle = charger.evses[0].connectors[0].vehicle.as_ref().unwrap();
        assert_eq!(vehicle.id, "MY-EV");
        assert_eq!(vehicle.state_of_charge, Some(55.0));
    }

    #[test]
    fn apply_ocpp_state_clears_the_vehicle_once_the_connector_frees_up() {
        let mut charger = charger_state();
        charger.evses[0].connectors[0].vehicle = Some(Vehicle {
            id: "MY-EV".into(),
            state_of_charge: None,
        });
        let ocpp = ocpp_state_with(vec![
            OcppConnectorState::Available,
            OcppConnectorState::Available,
        ]);

        apply_ocpp_state(&mut charger, &ocpp);

        assert!(charger.evses[0].connectors[0].vehicle.is_none());
    }

    #[test]
    fn apply_ocpp_state_does_not_invent_a_vehicle_for_a_reserved_connector() {
        // A reservation holds a connector for someone who hasn't arrived yet - nothing is
        // plugged in, so no vehicle should be synthesized.
        let mut charger = charger_state();
        let ocpp = ocpp_state_with(vec![
            OcppConnectorState::Reserved,
            OcppConnectorState::Available,
        ]);

        apply_ocpp_state(&mut charger, &ocpp);

        assert_eq!(
            charger.evses[0].connectors[0].status,
            ConnectorStatus::Reserved
        );
        assert!(charger.evses[0].connectors[0].vehicle.is_none());
    }

    #[test]
    fn plug_in_vehicle_maps_to_cable_connected_only_when_available() {
        assert_eq!(
            command_to_connector_event(Command::PlugInVehicle, OcppConnectorState::Available, ""),
            Some(ConnectorEvent::CableConnected)
        );
        assert_eq!(
            command_to_connector_event(Command::PlugInVehicle, OcppConnectorState::Connected, ""),
            None
        );
    }

    #[test]
    fn present_rfid_requires_locked_and_carries_the_given_tag() {
        assert_eq!(
            command_to_connector_event(Command::PresentRfid, OcppConnectorState::Locked, "TAG-1"),
            Some(ConnectorEvent::IdTokenPresented(IdToken {
                value: "TAG-1".into(),
                kind: IdTokenKind::ISO14443,
            }))
        );
        assert_eq!(
            command_to_connector_event(
                Command::PresentRfid,
                OcppConnectorState::Connected,
                "TAG-1"
            ),
            None
        );
    }

    #[test]
    fn unplug_vehicle_stops_charging_first_then_disconnects_once_back_to_connected() {
        assert_eq!(
            command_to_connector_event(Command::UnplugVehicle, OcppConnectorState::Charging, ""),
            Some(ConnectorEvent::ChargingStopped(StopReason::Local))
        );
        assert_eq!(
            command_to_connector_event(Command::UnplugVehicle, OcppConnectorState::Connected, ""),
            Some(ConnectorEvent::CableDisconnected)
        );
        assert_eq!(
            command_to_connector_event(Command::UnplugVehicle, OcppConnectorState::Locked, ""),
            None
        );
    }

    #[test]
    fn report_and_clear_fault_round_trip() {
        assert_eq!(
            command_to_connector_event(Command::ReportFault, OcppConnectorState::Available, ""),
            Some(ConnectorEvent::FaultDetected)
        );
        assert_eq!(
            command_to_connector_event(Command::ReportFault, OcppConnectorState::Faulted, ""),
            None
        );
        assert_eq!(
            command_to_connector_event(Command::ClearFault, OcppConnectorState::Faulted, ""),
            Some(ConnectorEvent::FaultCleared)
        );
        assert_eq!(
            command_to_connector_event(Command::ClearFault, OcppConnectorState::Available, ""),
            None
        );
    }

    #[test]
    fn build_ocpp_event_for_connector_targets_the_given_connector_even_when_an_earlier_one_would_also_be_eligible()
     {
        let ocpp = ocpp_state_with(vec![
            OcppConnectorState::Available,
            OcppConnectorState::Available,
        ]);

        let event =
            build_ocpp_event_for_connector(&ocpp, 0, 1, Command::PlugInVehicle, "").unwrap();

        assert_eq!(
            event,
            ChargePointEvent::Evse {
                evse_id: 0,
                event: EvseEvent::Connector {
                    connector_id: 1,
                    event: ConnectorEvent::CableConnected,
                },
            }
        );
    }

    #[test]
    fn build_ocpp_event_for_connector_returns_none_when_that_connector_is_not_eligible() {
        let ocpp = ocpp_state_with(vec![
            OcppConnectorState::Available,
            OcppConnectorState::Charging,
        ]);
        assert_eq!(
            build_ocpp_event_for_connector(&ocpp, 0, 1, Command::PlugInVehicle, ""),
            None
        );
    }

    #[test]
    fn build_ocpp_event_for_connector_returns_none_for_an_unknown_evse_or_connector_id() {
        let ocpp = ocpp_state_with(vec![OcppConnectorState::Available]);
        assert_eq!(
            build_ocpp_event_for_connector(&ocpp, 5, 0, Command::PlugInVehicle, ""),
            None
        );
        assert_eq!(
            build_ocpp_event_for_connector(&ocpp, 0, 5, Command::PlugInVehicle, ""),
            None
        );
    }

    // --- apply_hardware_state -------------------------------------------------------------

    use ocpp_charge_point::hardware::Connector;

    fn hardware_for(evses: Vec<EvseConfig>) -> FakeChargePoint {
        FakeChargePoint::from_config(&ChargerConfig {
            id: "CP001".into(),
            ocpp_version: OcppVersion::V21,
            evses,
            has_display: false,
            capabilities: CapabilitiesConfig::default(),
        })
    }

    #[tokio::test]
    async fn apply_hardware_state_reads_lock_and_contactor_off_the_matching_connector() {
        let hardware = hardware_for(vec![EvseConfig {
            id: 1,
            connectors: 2,
        }]);
        hardware.evses()[0].connectors()[0].lock().await.unwrap();
        hardware.evses()[0].connectors()[0]
            .close_contactor()
            .await
            .unwrap();

        let mut charger = charger_state();
        apply_hardware_state(&mut charger, &hardware);

        assert!(charger.evses[0].connectors[0].locked);
        assert!(charger.evses[0].connectors[0].contactor_closed);
        // The untouched sibling connector must not pick up the first one's state.
        assert!(!charger.evses[0].connectors[1].locked);
        assert!(!charger.evses[0].connectors[1].contactor_closed);
    }

    #[tokio::test]
    async fn apply_hardware_state_distinguishes_some_zero_from_none_for_the_current_limit() {
        let hardware = hardware_for(vec![EvseConfig {
            id: 1,
            connectors: 1,
        }]);
        let mut charger = charger_state();

        apply_hardware_state(&mut charger, &hardware);
        assert_eq!(charger.evses[0].connectors[0].current_limit_ma, None);

        hardware.evses()[0].connectors()[0]
            .set_current_limit(Some(0))
            .await
            .unwrap();
        apply_hardware_state(&mut charger, &hardware);
        assert_eq!(
            charger.evses[0].connectors[0].current_limit_ma,
            Some(0),
            "Some(0) (suspended) must not collapse into None (unlimited)"
        );

        hardware.evses()[0].connectors()[0]
            .set_current_limit(Some(16_000))
            .await
            .unwrap();
        apply_hardware_state(&mut charger, &hardware);
        assert_eq!(
            charger.evses[0].connectors[0].current_limit_ma,
            Some(16_000)
        );

        hardware.evses()[0].connectors()[0]
            .set_current_limit(None)
            .await
            .unwrap();
        apply_hardware_state(&mut charger, &hardware);
        assert_eq!(charger.evses[0].connectors[0].current_limit_ma, None);
    }

    /// The trap H3 hit (`docs/hardware-roadmap.md`): `EvseConfig::id` need not be 1-based or
    /// contiguous, but `charger.evses`/`hardware.evses()` are still built from the same config in
    /// the same order, so position - never `EvseConfig::id` or `FakeConnector`'s own
    /// `evse_id`/`connector_id` - must be what lines a hardware connector up with its
    /// `ConnectorState`.
    #[tokio::test]
    async fn apply_hardware_state_addresses_connectors_positionally_not_by_evse_config_id() {
        let evses = vec![
            EvseConfig {
                id: 5,
                connectors: 1,
            },
            EvseConfig {
                id: 2,
                connectors: 1,
            },
        ];
        let hardware = hardware_for(evses.clone());
        let mut charger = ChargerState::from_config(ChargerConfig {
            id: "CP001".into(),
            ocpp_version: OcppVersion::V21,
            evses,
            has_display: false,
            capabilities: CapabilitiesConfig::default(),
        });

        // Lock only the second positional EVSE's connector - id 2, not id 5.
        hardware.evses()[1].connectors()[0].lock().await.unwrap();

        apply_hardware_state(&mut charger, &hardware);

        assert!(
            !charger.evses[0].connectors[0].locked,
            "position 0 (EvseConfig::id 5) must stay unlocked"
        );
        assert!(
            charger.evses[1].connectors[0].locked,
            "position 1 (EvseConfig::id 2) must reflect the lock"
        );
    }

    /// H14: direction and the export register are hardware-only facts too, so they ride along in
    /// the same projection rather than needing a second one.
    #[tokio::test]
    async fn apply_hardware_state_reads_power_direction_and_exported_energy() {
        let hardware = hardware_for(vec![EvseConfig {
            id: 1,
            connectors: 2,
        }]);
        let mut charger = charger_state();

        apply_hardware_state(&mut charger, &hardware);
        assert!(!charger.evses[0].connectors[0].discharging);
        assert_eq!(charger.evses[0].connectors[0].exported_energy_wh, 0);

        // Discharge, with the contactor closed so the meter actually moves, then tick. The
        // connector is ticked directly rather than through `FakeChargePoint::tick`, which is a
        // deliberate no-op until `start` has stashed a `HardwareEventSender` (see its own test) -
        // this fixture has no runtime behind it, only hardware.
        let connector = &hardware.evses()[0].connectors()[0];
        connector.set_discharging(true);
        connector.close_contactor().await.unwrap();
        connector.tick(std::time::Duration::from_secs(3600));

        apply_hardware_state(&mut charger, &hardware);
        assert!(charger.evses[0].connectors[0].discharging);
        assert!(
            charger.evses[0].connectors[0].exported_energy_wh > 0,
            "an hour of simulated discharge should have exported something"
        );
        // The untouched sibling must not pick up either fact.
        assert!(!charger.evses[0].connectors[1].discharging);
        assert_eq!(charger.evses[0].connectors[1].exported_energy_wh, 0);
    }

    #[tokio::test]
    async fn a_snapshot_carries_the_same_projection_apply_hardware_state_applies_directly() {
        let evses = vec![
            EvseConfig {
                id: 1,
                connectors: 2,
            },
            EvseConfig {
                id: 2,
                connectors: 1,
            },
        ];
        let hardware = hardware_for(evses.clone());
        hardware.evses()[0].connectors()[1].lock().await.unwrap();
        hardware.evses()[1].connectors()[0]
            .set_current_limit(Some(0))
            .await
            .unwrap();
        hardware.evses()[1].connectors()[0].set_discharging(true);

        let config = ChargerConfig {
            id: "CP001".into(),
            ocpp_version: OcppVersion::V21,
            evses,
            has_display: false,
            capabilities: CapabilitiesConfig::default(),
        };
        let mut applied_directly = ChargerState::from_config(config.clone());
        apply_hardware_state(&mut applied_directly, &hardware);

        let mut applied_from_snapshot = ChargerState::from_config(config);
        apply_hardware_snapshot(&mut applied_from_snapshot, &hardware_snapshot(&hardware));

        assert_eq!(applied_from_snapshot.evses, applied_directly.evses);
        // Not vacuously equal: the facts set above have to have survived the round trip.
        assert!(applied_from_snapshot.evses[0].connectors[1].locked);
        assert_eq!(
            applied_from_snapshot.evses[1].connectors[0].current_limit_ma,
            Some(0)
        );
        assert!(applied_from_snapshot.evses[1].connectors[0].discharging);
    }

    /// A snapshot from a *different* charger (or one taken before a config changed) must not write
    /// a foreign connector's lock state onto this charger's connectors - see
    /// [`apply_hardware_snapshot`]'s doc comment. Anything the snapshot doesn't cover keeps what it
    /// had.
    #[test]
    fn applying_a_shorter_or_longer_snapshot_leaves_the_uncovered_connectors_alone() {
        let mut charger = ChargerState::from_config(ChargerConfig {
            id: "CP001".into(),
            ocpp_version: OcppVersion::V21,
            evses: vec![EvseConfig {
                id: 1,
                connectors: 2,
            }],
            has_display: false,
            capabilities: CapabilitiesConfig::default(),
        });
        charger.evses[0].connectors[1].locked = true;

        // One EVSE too many, and one connector too few on the EVSE that does line up.
        apply_hardware_snapshot(
            &mut charger,
            &[
                vec![ConnectorHardwareSnapshot {
                    contactor_closed: true,
                    ..Default::default()
                }],
                vec![ConnectorHardwareSnapshot::default()],
            ],
        );

        assert!(charger.evses[0].connectors[0].contactor_closed);
        assert!(
            charger.evses[0].connectors[1].locked,
            "a connector the snapshot said nothing about must keep its own state"
        );
    }
}
