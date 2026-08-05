//! Bridges a live, connected OCPP 2.1 charger's real protocol state (from `ocpp-charge-point`)
//! with the simulator's own coarse [`ChargerState`]/[`Command`] model. Pure mapping/decision
//! logic only - the thread and channel plumbing that carries values across this boundary lives
//! in the TUI, since it's integration glue rather than testable state logic.

use ocpp_charge_point::state::{
    ChargePointEvent, ChargePointState, ConnectorEvent, ConnectorState as OcppConnectorState,
    EvseEvent, IdToken, IdTokenKind, MeterSample, RegistrationStatus, StopReason,
};

use super::command::Command;
use super::state::{ChargerState, ConnectionStatus, ConnectorStatus, Vehicle};

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

/// Overwrites `charger`'s connection/connector status from a live `ChargePointState` snapshot,
/// keyed positionally (charger and OCPP EVSEs/connectors are built from the same config, in the
/// same order). Synthesizes a placeholder vehicle the first time a connector reports a
/// plugged-in state, and clears it once free again; a vehicle already known locally (e.g. named
/// via a "Plug in vehicle" parameter prompt) is left alone rather than overwritten every
/// snapshot.
pub fn apply_ocpp_state(charger: &mut ChargerState, ocpp_state: &ChargePointState) {
    charger.connection_status = map_connection_status(ocpp_state.registration);

    for (evse, ocpp_evse) in charger.evses.iter_mut().zip(ocpp_state.evses.iter()) {
        for (connector, &ocpp_connector) in
            evse.connectors.iter_mut().zip(ocpp_evse.connectors.iter())
        {
            connector.status = map_connector_status(ocpp_connector);
            if matches!(
                ocpp_connector,
                OcppConnectorState::Available | OcppConnectorState::Unavailable
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
        (Command::PlugInVehicle, OcppConnectorState::Available) => Some(ConnectorEvent::CableConnected),
        (Command::PresentRfid, OcppConnectorState::Locked) => {
            let value = if input.is_empty() { "UNKNOWN".to_string() } else { input.to_string() };
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
            if !matches!(state, OcppConnectorState::Faulted | OcppConnectorState::FaultedSafe) =>
        {
            Some(ConnectorEvent::FaultDetected)
        }
        (Command::ClearFault, OcppConnectorState::Faulted | OcppConnectorState::FaultedSafe) => {
            Some(ConnectorEvent::FaultCleared)
        }
        _ => None,
    }
}

/// Finds the first connector in `evse_id`'s live OCPP state eligible for `command` (mirroring
/// [`Command::apply`]'s "first eligible connector" rule) and builds the full event to send to
/// the runtime. Returns `None` if `evse_id` doesn't exist or no connector is eligible yet (e.g.
/// the connector hasn't finished a hardware handshake the coarse local status already shows as
/// done - a real connector reports "occupied" for `Connected` through `Unlocking` alike, so a
/// command needing a more specific fine-grained state can briefly appear available before it
/// actually is).
pub fn build_ocpp_event(
    ocpp_state: &ChargePointState,
    evse_id: usize,
    command: Command,
    input: &str,
) -> Option<ChargePointEvent> {
    let evse = ocpp_state.evses.get(evse_id)?;
    let (connector_id, event) = evse.connectors.iter().enumerate().find_map(|(index, &state)| {
        command_to_connector_event(command, state, input).map(|event| (index, event))
    })?;

    Some(ChargePointEvent::Evse {
        evse_id,
        event: EvseEvent::Connector { connector_id, event },
    })
}

/// Builds a `MeterValueSampled` event for every currently-`Charging` connector, carrying its
/// EVSE's simulated cumulative energy reading (see [`super::state::EvseState::tick`]). Real
/// meter values are per-connector; this simulator's energy simulation is aggregated per EVSE
/// (there's no per-connector meter), so every charging connector on the same EVSE reports that
/// EVSE's shared total - a known simplification, harmless for the common single-connector EVSE
/// case this crate's presets use.
pub fn meter_sample_events(charger: &ChargerState) -> Vec<ChargePointEvent> {
    charger
        .evses
        .iter()
        .enumerate()
        .flat_map(|(evse_id, evse)| {
            let energy_wh = (evse.metrics.energy_kwh * 1000.0).round() as i64;
            evse.connectors
                .iter()
                .enumerate()
                .filter(|(_, connector)| connector.status == ConnectorStatus::Charging)
                .map(move |(connector_id, _)| ChargePointEvent::Evse {
                    evse_id,
                    event: EvseEvent::Connector {
                        connector_id,
                        event: ConnectorEvent::MeterValueSampled(MeterSample { energy_wh }),
                    },
                })
        })
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::charger::config::{ChargerConfig, EvseConfig, OcppVersion};
    use crate::charger::state::{ChargerState, ConnectorStatus};
    use ocpp_charge_point::state::{EvseState as OcppEvseState, LifecycleState};

    fn charger_state() -> ChargerState {
        ChargerState::from_config(ChargerConfig {
            id: "CP001".into(),
            ocpp_version: OcppVersion::V21,
            evses: vec![EvseConfig { id: 1, connectors: 2 }],
            has_display: false,
        })
    }

    fn ocpp_state_with(connectors: Vec<OcppConnectorState>) -> ChargePointState {
        ChargePointState {
            lifecycle: LifecycleState::Available,
            registration: Some(RegistrationStatus::Accepted),
            evses: vec![OcppEvseState {
                status: ocpp_charge_point::state::EvseStatus::Available,
                connectors,
                transactions: vec![None, None],
            }],
            next_transaction_id: 0,
        }
    }

    #[test]
    fn maps_every_connector_state_to_a_coarse_status() {
        assert_eq!(map_connector_status(OcppConnectorState::Available), ConnectorStatus::Available);
        assert_eq!(map_connector_status(OcppConnectorState::Connected), ConnectorStatus::Occupied);
        assert_eq!(map_connector_status(OcppConnectorState::Locked), ConnectorStatus::Occupied);
        assert_eq!(map_connector_status(OcppConnectorState::Authorizing), ConnectorStatus::Occupied);
        assert_eq!(map_connector_status(OcppConnectorState::Starting), ConnectorStatus::Charging);
        assert_eq!(map_connector_status(OcppConnectorState::Charging), ConnectorStatus::Charging);
        assert_eq!(map_connector_status(OcppConnectorState::Stopping), ConnectorStatus::Occupied);
        assert_eq!(map_connector_status(OcppConnectorState::Finishing), ConnectorStatus::Occupied);
        assert_eq!(map_connector_status(OcppConnectorState::Unlocking), ConnectorStatus::Occupied);
        assert_eq!(map_connector_status(OcppConnectorState::Unavailable), ConnectorStatus::Unavailable);
        assert_eq!(map_connector_status(OcppConnectorState::Faulted), ConnectorStatus::Faulted);
        assert_eq!(map_connector_status(OcppConnectorState::FaultedSafe), ConnectorStatus::Faulted);
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
    fn apply_ocpp_state_updates_connection_and_connector_status() {
        let mut charger = charger_state();
        let ocpp = ocpp_state_with(vec![OcppConnectorState::Locked, OcppConnectorState::Available]);

        apply_ocpp_state(&mut charger, &ocpp);

        assert_eq!(charger.connection_status, ConnectionStatus::Connected);
        assert_eq!(charger.evses[0].connectors[0].status, ConnectorStatus::Occupied);
        assert_eq!(charger.evses[0].connectors[1].status, ConnectorStatus::Available);
    }

    #[test]
    fn apply_ocpp_state_synthesizes_a_vehicle_the_first_time_a_connector_becomes_occupied() {
        let mut charger = charger_state();
        let ocpp = ocpp_state_with(vec![OcppConnectorState::Connected, OcppConnectorState::Available]);

        apply_ocpp_state(&mut charger, &ocpp);

        assert!(charger.evses[0].connectors[0].vehicle.is_some());
        assert!(charger.evses[0].connectors[1].vehicle.is_none());
    }

    #[test]
    fn apply_ocpp_state_leaves_an_already_known_vehicle_untouched() {
        let mut charger = charger_state();
        charger.evses[0].connectors[0].vehicle = Some(Vehicle {
            id: "MY-EV".into(),
            state_of_charge: Some(55),
        });
        let ocpp = ocpp_state_with(vec![OcppConnectorState::Charging, OcppConnectorState::Available]);

        apply_ocpp_state(&mut charger, &ocpp);

        let vehicle = charger.evses[0].connectors[0].vehicle.as_ref().unwrap();
        assert_eq!(vehicle.id, "MY-EV");
        assert_eq!(vehicle.state_of_charge, Some(55));
    }

    #[test]
    fn apply_ocpp_state_clears_the_vehicle_once_the_connector_frees_up() {
        let mut charger = charger_state();
        charger.evses[0].connectors[0].vehicle = Some(Vehicle {
            id: "MY-EV".into(),
            state_of_charge: None,
        });
        let ocpp = ocpp_state_with(vec![OcppConnectorState::Available, OcppConnectorState::Available]);

        apply_ocpp_state(&mut charger, &ocpp);

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
            command_to_connector_event(Command::PresentRfid, OcppConnectorState::Connected, "TAG-1"),
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
    fn build_ocpp_event_targets_the_first_eligible_connector() {
        let ocpp = ocpp_state_with(vec![OcppConnectorState::Locked, OcppConnectorState::Available]);

        let event = build_ocpp_event(&ocpp, 0, Command::PlugInVehicle, "").unwrap();

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
    fn build_ocpp_event_returns_none_when_no_connector_is_eligible() {
        let ocpp = ocpp_state_with(vec![OcppConnectorState::Locked, OcppConnectorState::Charging]);
        assert_eq!(build_ocpp_event(&ocpp, 0, Command::PlugInVehicle, ""), None);
    }

    #[test]
    fn build_ocpp_event_returns_none_for_an_unknown_evse_id() {
        let ocpp = ocpp_state_with(vec![OcppConnectorState::Available]);
        assert_eq!(build_ocpp_event(&ocpp, 5, Command::PlugInVehicle, ""), None);
    }

    #[test]
    fn meter_sample_events_reports_only_charging_connectors() {
        let mut charger = charger_state();
        charger.evses[0].connectors[0].status = ConnectorStatus::Charging;
        charger.evses[0].connectors[1].status = ConnectorStatus::Available;
        charger.evses[0].metrics.energy_kwh = 1.5;

        let events = meter_sample_events(&charger);

        assert_eq!(
            events,
            vec![ChargePointEvent::Evse {
                evse_id: 0,
                event: EvseEvent::Connector {
                    connector_id: 0,
                    event: ConnectorEvent::MeterValueSampled(MeterSample { energy_wh: 1500 }),
                },
            }]
        );
    }

    #[test]
    fn meter_sample_events_is_empty_when_nothing_is_charging() {
        let charger = charger_state();
        assert_eq!(meter_sample_events(&charger), Vec::new());
    }

    #[test]
    fn meter_sample_events_reports_every_charging_connector_on_an_evse() {
        let mut charger = charger_state();
        charger.evses[0].connectors[0].status = ConnectorStatus::Charging;
        charger.evses[0].connectors[1].status = ConnectorStatus::Charging;
        charger.evses[0].metrics.energy_kwh = 2.0;

        let events = meter_sample_events(&charger);

        assert_eq!(events.len(), 2);
        assert!(events.iter().all(|event| matches!(
            event,
            ChargePointEvent::Evse {
                event: EvseEvent::Connector {
                    event: ConnectorEvent::MeterValueSampled(MeterSample { energy_wh: 2000 }),
                    ..
                },
                ..
            }
        )));
    }
}
