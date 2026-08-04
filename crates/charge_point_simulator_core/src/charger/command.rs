use super::state::{ConnectorStatus, EvseState, Vehicle};

/// A simulated real-world event that can be dispatched against an EVSE, e.g.
/// a vehicle plugging in or a connector faulting. Each command targets the
/// first connector within the EVSE that's in an eligible state for it.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Command {
    PlugInVehicle,
    PresentRfid,
    UnplugVehicle,
    ReportFault,
    ClearFault,
}

impl Command {
    pub const ALL: [Command; 5] = [
        Command::PlugInVehicle,
        Command::PresentRfid,
        Command::UnplugVehicle,
        Command::ReportFault,
        Command::ClearFault,
    ];

    pub fn label(&self) -> &'static str {
        match self {
            Command::PlugInVehicle => "Plug in vehicle",
            Command::PresentRfid => "Present RFID card",
            Command::UnplugVehicle => "Unplug vehicle",
            Command::ReportFault => "Report fault",
            Command::ClearFault => "Clear fault",
        }
    }

    /// Whether `evse` has at least one connector this command can act on.
    pub fn is_available(&self, evse: &EvseState) -> bool {
        evse.connectors
            .iter()
            .any(|connector| self.applies_to(connector.status))
    }

    fn applies_to(&self, status: ConnectorStatus) -> bool {
        match self {
            Command::PlugInVehicle => status == ConnectorStatus::Available,
            Command::PresentRfid => status == ConnectorStatus::Occupied,
            Command::UnplugVehicle => {
                matches!(status, ConnectorStatus::Occupied | ConnectorStatus::Charging)
            }
            Command::ReportFault => status != ConnectorStatus::Faulted,
            Command::ClearFault => status == ConnectorStatus::Faulted,
        }
    }

    /// Applies this command to the first eligible connector in `evse`,
    /// mutating its state and returning a human-readable log line. Returns
    /// `None` (and mutates nothing) if no connector is eligible.
    pub fn apply(&self, evse: &mut EvseState) -> Option<String> {
        let status_applies = |status| self.applies_to(status);
        let connector = evse
            .connectors
            .iter_mut()
            .find(|connector| status_applies(connector.status))?;

        let message = match self {
            Command::PlugInVehicle => {
                let vehicle_id = format!("EV-E{}C{}", evse.id, connector.id);
                connector.status = ConnectorStatus::Occupied;
                connector.vehicle = Some(Vehicle {
                    id: vehicle_id.clone(),
                    state_of_charge: Some(20),
                });
                format!(
                    "EVSE {} connector {}: vehicle {} plugged in",
                    evse.id, connector.id, vehicle_id
                )
            }
            Command::PresentRfid => {
                connector.status = ConnectorStatus::Charging;
                format!(
                    "EVSE {} connector {}: RFID presented, charging started",
                    evse.id, connector.id
                )
            }
            Command::UnplugVehicle => {
                let vehicle_id = connector
                    .vehicle
                    .take()
                    .map(|vehicle| vehicle.id)
                    .unwrap_or_else(|| "vehicle".to_string());
                connector.status = ConnectorStatus::Available;
                format!(
                    "EVSE {} connector {}: {} unplugged",
                    evse.id, connector.id, vehicle_id
                )
            }
            Command::ReportFault => {
                connector.status = ConnectorStatus::Faulted;
                format!("EVSE {} connector {}: fault reported", evse.id, connector.id)
            }
            Command::ClearFault => {
                connector.status = ConnectorStatus::Available;
                format!("EVSE {} connector {}: fault cleared", evse.id, connector.id)
            }
        };

        Some(message)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::charger::state::ConnectorState;

    fn evse_with_statuses(statuses: &[ConnectorStatus]) -> EvseState {
        EvseState {
            id: 1,
            connectors: statuses
                .iter()
                .enumerate()
                .map(|(index, &status)| ConnectorState {
                    id: index as u32 + 1,
                    status,
                    vehicle: None,
                })
                .collect(),
            metrics: Default::default(),
        }
    }

    #[test]
    fn plug_in_vehicle_is_only_available_with_a_free_connector() {
        let free = evse_with_statuses(&[ConnectorStatus::Available]);
        assert!(Command::PlugInVehicle.is_available(&free));

        let occupied = evse_with_statuses(&[ConnectorStatus::Occupied]);
        assert!(!Command::PlugInVehicle.is_available(&occupied));
    }

    #[test]
    fn plug_in_vehicle_occupies_the_first_free_connector_with_a_vehicle() {
        let mut evse = evse_with_statuses(&[ConnectorStatus::Occupied, ConnectorStatus::Available]);
        let message = Command::PlugInVehicle.apply(&mut evse).unwrap();

        assert_eq!(evse.connectors[1].status, ConnectorStatus::Occupied);
        assert!(evse.connectors[1].vehicle.is_some());
        assert!(message.contains("plugged in"));
    }

    #[test]
    fn present_rfid_starts_charging_on_an_occupied_connector() {
        let mut evse = evse_with_statuses(&[ConnectorStatus::Occupied]);
        Command::PresentRfid.apply(&mut evse).unwrap();
        assert_eq!(evse.connectors[0].status, ConnectorStatus::Charging);
    }

    #[test]
    fn present_rfid_is_unavailable_without_an_occupied_connector() {
        let evse = evse_with_statuses(&[ConnectorStatus::Available, ConnectorStatus::Charging]);
        assert!(!Command::PresentRfid.is_available(&evse));
    }

    #[test]
    fn unplug_vehicle_clears_the_vehicle_and_frees_the_connector() {
        let mut evse = evse_with_statuses(&[ConnectorStatus::Charging]);
        evse.connectors[0].vehicle = Some(Vehicle {
            id: "EV-1".into(),
            state_of_charge: Some(80),
        });

        let message = Command::UnplugVehicle.apply(&mut evse).unwrap();

        assert_eq!(evse.connectors[0].status, ConnectorStatus::Available);
        assert_eq!(evse.connectors[0].vehicle, None);
        assert!(message.contains("EV-1"));
    }

    #[test]
    fn unplug_vehicle_is_available_for_occupied_or_charging_connectors() {
        assert!(Command::UnplugVehicle.is_available(&evse_with_statuses(&[ConnectorStatus::Occupied])));
        assert!(Command::UnplugVehicle.is_available(&evse_with_statuses(&[ConnectorStatus::Charging])));
        assert!(!Command::UnplugVehicle.is_available(&evse_with_statuses(&[ConnectorStatus::Available])));
    }

    #[test]
    fn report_fault_faults_the_first_non_faulted_connector() {
        let mut evse = evse_with_statuses(&[ConnectorStatus::Faulted, ConnectorStatus::Available]);
        Command::ReportFault.apply(&mut evse).unwrap();
        assert_eq!(evse.connectors[1].status, ConnectorStatus::Faulted);
    }

    #[test]
    fn report_fault_is_unavailable_when_every_connector_is_already_faulted() {
        let evse = evse_with_statuses(&[ConnectorStatus::Faulted]);
        assert!(!Command::ReportFault.is_available(&evse));
    }

    #[test]
    fn clear_fault_restores_a_faulted_connector_to_available() {
        let mut evse = evse_with_statuses(&[ConnectorStatus::Faulted]);
        Command::ClearFault.apply(&mut evse).unwrap();
        assert_eq!(evse.connectors[0].status, ConnectorStatus::Available);
    }

    #[test]
    fn clear_fault_is_unavailable_without_a_faulted_connector() {
        let evse = evse_with_statuses(&[ConnectorStatus::Available]);
        assert!(!Command::ClearFault.is_available(&evse));
    }

    #[test]
    fn apply_returns_none_and_mutates_nothing_when_no_connector_is_eligible() {
        let mut evse = evse_with_statuses(&[ConnectorStatus::Available]);
        let before = evse.clone();

        let result = Command::PresentRfid.apply(&mut evse);

        assert_eq!(result, None);
        assert_eq!(evse, before);
    }
}
