use super::state::{ChargerState, ConnectorStatus, EvseState, Vehicle};

/// A simulated real-world event that can be dispatched against an EVSE, e.g.
/// a vehicle plugging in or a connector faulting. Each command targets the
/// first connector within the EVSE that's in an eligible state for it.
///
/// `SetDisplayMessage`/`ClearDisplayMessage` are the exception: a charger's display isn't
/// per-EVSE, so those two target the charger as a whole (see
/// [`Command::is_display_command`]/[`Command::is_available_for_charger`]/[`Command::apply_to_charger`]
/// instead of the EVSE-scoped methods).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Command {
    PlugInVehicle,
    PresentRfid,
    UnplugVehicle,
    ReportFault,
    ClearFault,
    SetDisplayMessage,
    ClearDisplayMessage,
}

/// A single free-text value a [`Command`] needs before it can be applied,
/// collected from the user via a parameter prompt.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum CommandParameter {
    VehicleId,
    RfidTag,
    FaultCode,
    DisplayMessage,
}

impl CommandParameter {
    /// The parameter prompt's title, and a placeholder shown when the field is empty.
    pub fn label(&self) -> &'static str {
        match self {
            CommandParameter::VehicleId => "Vehicle ID",
            CommandParameter::RfidTag => "RFID tag",
            CommandParameter::FaultCode => "Fault code",
            CommandParameter::DisplayMessage => "Display message",
        }
    }
}

impl Command {
    pub const ALL: [Command; 7] = [
        Command::PlugInVehicle,
        Command::PresentRfid,
        Command::UnplugVehicle,
        Command::ReportFault,
        Command::ClearFault,
        Command::SetDisplayMessage,
        Command::ClearDisplayMessage,
    ];

    pub fn label(&self) -> &'static str {
        match self {
            Command::PlugInVehicle => "Plug in vehicle",
            Command::PresentRfid => "Present RFID card",
            Command::UnplugVehicle => "Unplug vehicle",
            Command::ReportFault => "Report fault",
            Command::ClearFault => "Clear fault",
            Command::SetDisplayMessage => "Set display message",
            Command::ClearDisplayMessage => "Clear display message",
        }
    }

    /// The parameter this command prompts for before it can be applied, if any.
    pub fn parameter(&self) -> Option<CommandParameter> {
        match self {
            Command::PlugInVehicle => Some(CommandParameter::VehicleId),
            Command::PresentRfid => Some(CommandParameter::RfidTag),
            Command::ReportFault => Some(CommandParameter::FaultCode),
            Command::SetDisplayMessage => Some(CommandParameter::DisplayMessage),
            Command::UnplugVehicle | Command::ClearFault | Command::ClearDisplayMessage => None,
        }
    }

    /// Whether this command targets the charger's display as a whole rather than a specific
    /// EVSE - `SetDisplayMessage`/`ClearDisplayMessage` use
    /// [`Self::is_available_for_charger`]/[`Self::apply_to_charger`] instead of the EVSE-scoped
    /// [`Self::is_available`]/[`Self::apply`].
    pub fn is_display_command(&self) -> bool {
        matches!(self, Command::SetDisplayMessage | Command::ClearDisplayMessage)
    }

    /// Whether `charger` is eligible for this display command: it needs a display, and
    /// (for `ClearDisplayMessage`) an actual message showing.
    pub fn is_available_for_charger(&self, charger: &ChargerState) -> bool {
        if !charger.config.has_display {
            return false;
        }
        match self {
            Command::SetDisplayMessage => true,
            Command::ClearDisplayMessage => charger.display_message.is_some(),
            _ => false,
        }
    }

    /// Applies a display command to `charger`, returning a human-readable log line. Returns
    /// `None` if this isn't a display command, or the charger has no display.
    pub fn apply_to_charger(&self, charger: &mut ChargerState, input: &str) -> Option<String> {
        if !charger.config.has_display {
            return None;
        }
        match self {
            Command::SetDisplayMessage => {
                let input = input.trim();
                let message = if input.is_empty() { "Welcome" } else { input };
                charger.display_message = Some(message.to_string());
                Some(format!("display message set: \"{message}\""))
            }
            Command::ClearDisplayMessage => {
                charger.display_message = None;
                Some("display message cleared".to_string())
            }
            _ => None,
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
            // Display commands target the charger as a whole, not a connector - see
            // `is_available_for_charger` instead.
            Command::SetDisplayMessage | Command::ClearDisplayMessage => false,
        }
    }

    /// Applies this command to the first eligible connector in `evse`,
    /// mutating its state and returning a human-readable log line. Returns
    /// `None` (and mutates nothing) if no connector is eligible.
    ///
    /// `input` is the value collected for this command's [`parameter`](Self::parameter),
    /// or blank for commands that don't have one. A blank value falls back to a
    /// generated default rather than rejecting the command.
    pub fn apply(&self, evse: &mut EvseState, input: &str) -> Option<String> {
        let status_applies = |status| self.applies_to(status);
        let connector = evse
            .connectors
            .iter_mut()
            .find(|connector| status_applies(connector.status))?;
        let input = input.trim();

        let message = match self {
            Command::PlugInVehicle => {
                let vehicle_id = if input.is_empty() {
                    format!("EV-E{}C{}", evse.id, connector.id)
                } else {
                    input.to_string()
                };
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
                let tag = if input.is_empty() { "unknown" } else { input };
                connector.status = ConnectorStatus::Charging;
                format!(
                    "EVSE {} connector {}: RFID {} presented, charging started",
                    evse.id, connector.id, tag
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
                let code = if input.is_empty() { "GenericError" } else { input };
                connector.status = ConnectorStatus::Faulted;
                format!(
                    "EVSE {} connector {}: fault reported ({})",
                    evse.id, connector.id, code
                )
            }
            Command::ClearFault => {
                connector.status = ConnectorStatus::Available;
                format!("EVSE {} connector {}: fault cleared", evse.id, connector.id)
            }
            // `applies_to` always returns `false` for these, so the `?` above already
            // returned before a connector could ever be found for one.
            Command::SetDisplayMessage | Command::ClearDisplayMessage => unreachable!(
                "display commands never match a connector via applies_to"
            ),
        };

        Some(message)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::charger::config::{ChargerConfig, OcppVersion};
    use crate::charger::state::ConnectorState;

    fn charger_with_display(has_display: bool) -> ChargerState {
        ChargerState::from_config(ChargerConfig {
            id: "CP001".into(),
            ocpp_version: OcppVersion::V21,
            evses: vec![],
            has_display,
        })
    }

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
        let message = Command::PlugInVehicle.apply(&mut evse, "").unwrap();

        assert_eq!(evse.connectors[1].status, ConnectorStatus::Occupied);
        assert!(evse.connectors[1].vehicle.is_some());
        assert!(message.contains("plugged in"));
    }

    #[test]
    fn plug_in_vehicle_uses_the_given_vehicle_id_when_provided() {
        let mut evse = evse_with_statuses(&[ConnectorStatus::Available]);
        let message = Command::PlugInVehicle.apply(&mut evse, "MY-EV-1").unwrap();

        assert_eq!(evse.connectors[0].vehicle.as_ref().unwrap().id, "MY-EV-1");
        assert!(message.contains("MY-EV-1"));
    }

    #[test]
    fn plug_in_vehicle_falls_back_to_a_generated_id_when_left_blank() {
        let mut evse = evse_with_statuses(&[ConnectorStatus::Available]);
        Command::PlugInVehicle.apply(&mut evse, "  ").unwrap();

        assert_eq!(evse.connectors[0].vehicle.as_ref().unwrap().id, "EV-E1C1");
    }

    #[test]
    fn present_rfid_starts_charging_on_an_occupied_connector() {
        let mut evse = evse_with_statuses(&[ConnectorStatus::Occupied]);
        Command::PresentRfid.apply(&mut evse, "").unwrap();
        assert_eq!(evse.connectors[0].status, ConnectorStatus::Charging);
    }

    #[test]
    fn present_rfid_includes_the_given_tag_in_the_log_line() {
        let mut evse = evse_with_statuses(&[ConnectorStatus::Occupied]);
        let message = Command::PresentRfid.apply(&mut evse, "TAG-42").unwrap();
        assert!(message.contains("TAG-42"));
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

        let message = Command::UnplugVehicle.apply(&mut evse, "").unwrap();

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
        Command::ReportFault.apply(&mut evse, "").unwrap();
        assert_eq!(evse.connectors[1].status, ConnectorStatus::Faulted);
    }

    #[test]
    fn report_fault_includes_the_given_fault_code_in_the_log_line() {
        let mut evse = evse_with_statuses(&[ConnectorStatus::Available]);
        let message = Command::ReportFault.apply(&mut evse, "OverCurrentFailure").unwrap();
        assert!(message.contains("OverCurrentFailure"));
    }

    #[test]
    fn report_fault_is_unavailable_when_every_connector_is_already_faulted() {
        let evse = evse_with_statuses(&[ConnectorStatus::Faulted]);
        assert!(!Command::ReportFault.is_available(&evse));
    }

    #[test]
    fn clear_fault_restores_a_faulted_connector_to_available() {
        let mut evse = evse_with_statuses(&[ConnectorStatus::Faulted]);
        Command::ClearFault.apply(&mut evse, "").unwrap();
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

        let result = Command::PresentRfid.apply(&mut evse, "");

        assert_eq!(result, None);
        assert_eq!(evse, before);
    }

    #[test]
    fn only_commands_that_need_extra_input_report_a_parameter() {
        assert_eq!(Command::PlugInVehicle.parameter(), Some(CommandParameter::VehicleId));
        assert_eq!(Command::PresentRfid.parameter(), Some(CommandParameter::RfidTag));
        assert_eq!(Command::ReportFault.parameter(), Some(CommandParameter::FaultCode));
        assert_eq!(Command::UnplugVehicle.parameter(), None);
        assert_eq!(Command::ClearFault.parameter(), None);
        assert_eq!(
            Command::SetDisplayMessage.parameter(),
            Some(CommandParameter::DisplayMessage)
        );
        assert_eq!(Command::ClearDisplayMessage.parameter(), None);
    }

    #[test]
    fn only_the_display_commands_are_flagged_as_display_commands() {
        assert!(Command::SetDisplayMessage.is_display_command());
        assert!(Command::ClearDisplayMessage.is_display_command());
        assert!(!Command::PlugInVehicle.is_display_command());
        assert!(!Command::ReportFault.is_display_command());
    }

    #[test]
    fn set_display_message_needs_a_display() {
        assert!(!Command::SetDisplayMessage.is_available_for_charger(&charger_with_display(false)));
        assert!(Command::SetDisplayMessage.is_available_for_charger(&charger_with_display(true)));
    }

    #[test]
    fn clear_display_message_also_needs_a_message_actually_showing() {
        let mut charger = charger_with_display(true);
        assert!(!Command::ClearDisplayMessage.is_available_for_charger(&charger));

        charger.display_message = Some("hello".to_string());
        assert!(Command::ClearDisplayMessage.is_available_for_charger(&charger));
    }

    #[test]
    fn set_display_message_stores_the_given_text() {
        let mut charger = charger_with_display(true);
        let message = Command::SetDisplayMessage.apply_to_charger(&mut charger, "Welcome to Flowion").unwrap();

        assert_eq!(charger.display_message, Some("Welcome to Flowion".to_string()));
        assert!(message.contains("Welcome to Flowion"));
    }

    #[test]
    fn set_display_message_falls_back_to_a_default_when_left_blank() {
        let mut charger = charger_with_display(true);
        Command::SetDisplayMessage.apply_to_charger(&mut charger, "  ").unwrap();

        assert_eq!(charger.display_message, Some("Welcome".to_string()));
    }

    #[test]
    fn clear_display_message_blanks_the_message() {
        let mut charger = charger_with_display(true);
        charger.display_message = Some("hello".to_string());

        Command::ClearDisplayMessage.apply_to_charger(&mut charger, "").unwrap();

        assert_eq!(charger.display_message, None);
    }

    #[test]
    fn display_commands_do_nothing_on_a_charger_without_a_display() {
        let mut charger = charger_with_display(false);
        let result = Command::SetDisplayMessage.apply_to_charger(&mut charger, "hi");

        assert_eq!(result, None);
        assert_eq!(charger.display_message, None);
    }
}
