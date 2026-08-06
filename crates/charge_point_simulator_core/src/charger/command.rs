use super::state::{ChargerState, ConnectorState, ConnectorStatus, EvseState, Vehicle};

/// A simulated real-world event that can be dispatched against a connector, e.g.
/// a vehicle plugging in or a connector faulting.
/// [`Self::is_available_for_connector`]/[`Self::apply_to`] target one specific connector by index:
/// callers (e.g. a dashboard focused on a single connector) would have a bug if a command
/// silently acted on a different connector than the one the user is looking at.
///
/// `SetDisplayMessage`/`ClearDisplayMessage` are the exception: a charger's display isn't
/// per-connector, so those two target the charger as a whole (see
/// [`Command::is_display_command`]/[`Command::is_available_for_charger`]/[`Command::apply_to_charger`]
/// instead of the connector-scoped methods).
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
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
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

    /// An example value, shown as a placeholder in an empty prompt field. Deliberately a real
    /// example rather than a restatement of [`Self::label`], which the prompt's title already
    /// carries - a placeholder that repeats the title tells the user nothing.
    pub fn placeholder(&self) -> &'static str {
        match self {
            CommandParameter::VehicleId => "e.g. MY-EV-1",
            CommandParameter::RfidTag => "e.g. TAG-42",
            // `SuspendedEV` and friends are the OCPP-defined vocabulary a CSMS expects here.
            CommandParameter::FaultCode => "e.g. GroundFailure",
            CommandParameter::DisplayMessage => "e.g. Charging - 80% complete",
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

    /// One short sentence saying what this command does to the targeted connector.
    pub fn description(&self) -> &'static str {
        match self {
            Command::PlugInVehicle => "Occupies a free connector with a vehicle",
            Command::PresentRfid => "Authorizes charging on an occupied connector",
            Command::UnplugVehicle => "Removes the vehicle and frees the connector",
            Command::ReportFault => "Marks the connector as faulted",
            Command::ClearFault => "Restores a faulted connector to available",
            Command::SetDisplayMessage => "Shows text on the charger's display",
            Command::ClearDisplayMessage => "Blanks the charger's display",
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
    /// connector - `SetDisplayMessage`/`ClearDisplayMessage` use
    /// [`Self::is_available_for_charger`]/[`Self::apply_to_charger`] instead of the
    /// connector-scoped [`Self::is_available_for_connector`]/[`Self::apply_to`].
    pub fn is_display_command(&self) -> bool {
        matches!(
            self,
            Command::SetDisplayMessage | Command::ClearDisplayMessage
        )
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

    fn applies_to(&self, status: ConnectorStatus) -> bool {
        match self {
            Command::PlugInVehicle => status == ConnectorStatus::Available,
            Command::PresentRfid => status == ConnectorStatus::Occupied,
            Command::UnplugVehicle => {
                matches!(
                    status,
                    ConnectorStatus::Occupied | ConnectorStatus::Charging
                )
            }
            Command::ReportFault => status != ConnectorStatus::Faulted,
            Command::ClearFault => status == ConnectorStatus::Faulted,
            // Display commands target the charger as a whole, not a connector - see
            // `is_available_for_charger` instead.
            Command::SetDisplayMessage | Command::ClearDisplayMessage => false,
        }
    }

    /// Whether `connector` is individually eligible for this command, so callers can check (or
    /// explain) one specific connector before dispatching against it.
    pub fn is_available_for_connector(&self, connector: &ConnectorState) -> bool {
        self.applies_to(connector.status)
    }

    /// Applies this command to the connector at `connector_index` within `evse`, mutating its
    /// state and returning a human-readable log line. Returns `None` (and mutates nothing) if
    /// `connector_index` is out of range or that connector isn't eligible for this command.
    ///
    /// `input` is the value collected for this command's [`parameter`](Self::parameter),
    /// or blank for commands that don't have one. A blank value falls back to a
    /// generated default rather than rejecting the command.
    pub fn apply_to(
        &self,
        evse: &mut EvseState,
        connector_index: usize,
        input: &str,
    ) -> Option<String> {
        let evse_id = evse.id;
        let connector = evse.connectors.get_mut(connector_index)?;
        if !self.applies_to(connector.status) {
            return None;
        }
        let input = input.trim();

        let message = match self {
            Command::PlugInVehicle => {
                let vehicle_id = if input.is_empty() {
                    format!("EV-E{}C{}", evse_id, connector.id)
                } else {
                    input.to_string()
                };
                connector.status = ConnectorStatus::Occupied;
                connector.vehicle = Some(Vehicle {
                    id: vehicle_id.clone(),
                    state_of_charge: Some(20.0),
                });
                format!(
                    "EVSE {} connector {}: vehicle {} plugged in",
                    evse_id, connector.id, vehicle_id
                )
            }
            Command::PresentRfid => {
                let tag = if input.is_empty() { "unknown" } else { input };
                connector.status = ConnectorStatus::Charging;
                format!(
                    "EVSE {} connector {}: RFID {} presented, charging started",
                    evse_id, connector.id, tag
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
                    evse_id, connector.id, vehicle_id
                )
            }
            Command::ReportFault => {
                let code = if input.is_empty() {
                    "GenericError"
                } else {
                    input
                };
                connector.status = ConnectorStatus::Faulted;
                format!(
                    "EVSE {} connector {}: fault reported ({})",
                    evse_id, connector.id, code
                )
            }
            Command::ClearFault => {
                connector.status = ConnectorStatus::Available;
                format!("EVSE {} connector {}: fault cleared", evse_id, connector.id)
            }
            // `applies_to` always returns `false` for these, so the check above already
            // returned before this point could ever be reached for one.
            Command::SetDisplayMessage | Command::ClearDisplayMessage => {
                unreachable!("display commands never match a connector via applies_to")
            }
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
                    session_duration: std::time::Duration::ZERO,
                })
                .collect(),
            metrics: Default::default(),
        }
    }

    #[test]
    fn plug_in_vehicle_is_only_available_for_a_free_connector() {
        let evse = evse_with_statuses(&[ConnectorStatus::Available, ConnectorStatus::Occupied]);

        assert!(Command::PlugInVehicle.is_available_for_connector(&evse.connectors[0]));
        assert!(!Command::PlugInVehicle.is_available_for_connector(&evse.connectors[1]));
    }

    #[test]
    fn plug_in_vehicle_occupies_the_connector_with_a_vehicle() {
        let mut evse = evse_with_statuses(&[ConnectorStatus::Occupied, ConnectorStatus::Available]);
        let message = Command::PlugInVehicle.apply_to(&mut evse, 1, "").unwrap();

        assert_eq!(evse.connectors[1].status, ConnectorStatus::Occupied);
        assert!(evse.connectors[1].vehicle.is_some());
        assert!(message.contains("plugged in"));
    }

    #[test]
    fn plug_in_vehicle_uses_the_given_vehicle_id_when_provided() {
        let mut evse = evse_with_statuses(&[ConnectorStatus::Available]);
        let message = Command::PlugInVehicle
            .apply_to(&mut evse, 0, "MY-EV-1")
            .unwrap();

        assert_eq!(evse.connectors[0].vehicle.as_ref().unwrap().id, "MY-EV-1");
        assert!(message.contains("MY-EV-1"));
    }

    #[test]
    fn plug_in_vehicle_falls_back_to_a_generated_id_when_left_blank() {
        let mut evse = evse_with_statuses(&[ConnectorStatus::Available]);
        Command::PlugInVehicle.apply_to(&mut evse, 0, "  ").unwrap();

        assert_eq!(evse.connectors[0].vehicle.as_ref().unwrap().id, "EV-E1C1");
    }

    #[test]
    fn present_rfid_starts_charging_on_an_occupied_connector() {
        let mut evse = evse_with_statuses(&[ConnectorStatus::Occupied]);
        Command::PresentRfid.apply_to(&mut evse, 0, "").unwrap();
        assert_eq!(evse.connectors[0].status, ConnectorStatus::Charging);
    }

    #[test]
    fn present_rfid_includes_the_given_tag_in_the_log_line() {
        let mut evse = evse_with_statuses(&[ConnectorStatus::Occupied]);
        let message = Command::PresentRfid
            .apply_to(&mut evse, 0, "TAG-42")
            .unwrap();
        assert!(message.contains("TAG-42"));
    }

    #[test]
    fn present_rfid_is_unavailable_for_a_connector_that_is_not_occupied() {
        let evse = evse_with_statuses(&[ConnectorStatus::Available, ConnectorStatus::Charging]);
        assert!(!Command::PresentRfid.is_available_for_connector(&evse.connectors[0]));
        assert!(!Command::PresentRfid.is_available_for_connector(&evse.connectors[1]));
    }

    #[test]
    fn unplug_vehicle_clears_the_vehicle_and_frees_the_connector() {
        let mut evse = evse_with_statuses(&[ConnectorStatus::Charging]);
        evse.connectors[0].vehicle = Some(Vehicle {
            id: "EV-1".into(),
            state_of_charge: Some(80.0),
        });

        let message = Command::UnplugVehicle.apply_to(&mut evse, 0, "").unwrap();

        assert_eq!(evse.connectors[0].status, ConnectorStatus::Available);
        assert_eq!(evse.connectors[0].vehicle, None);
        assert!(message.contains("EV-1"));
    }

    #[test]
    fn unplug_vehicle_is_available_for_occupied_or_charging_connectors() {
        let evse = evse_with_statuses(&[
            ConnectorStatus::Occupied,
            ConnectorStatus::Charging,
            ConnectorStatus::Available,
        ]);

        assert!(Command::UnplugVehicle.is_available_for_connector(&evse.connectors[0]));
        assert!(Command::UnplugVehicle.is_available_for_connector(&evse.connectors[1]));
        assert!(!Command::UnplugVehicle.is_available_for_connector(&evse.connectors[2]));
    }

    #[test]
    fn report_fault_faults_the_targeted_connector() {
        let mut evse = evse_with_statuses(&[ConnectorStatus::Faulted, ConnectorStatus::Available]);
        Command::ReportFault.apply_to(&mut evse, 1, "").unwrap();
        assert_eq!(evse.connectors[1].status, ConnectorStatus::Faulted);
    }

    #[test]
    fn report_fault_includes_the_given_fault_code_in_the_log_line() {
        let mut evse = evse_with_statuses(&[ConnectorStatus::Available]);
        let message = Command::ReportFault
            .apply_to(&mut evse, 0, "OverCurrentFailure")
            .unwrap();
        assert!(message.contains("OverCurrentFailure"));
    }

    #[test]
    fn report_fault_is_unavailable_for_an_already_faulted_connector() {
        let evse = evse_with_statuses(&[ConnectorStatus::Faulted]);
        assert!(!Command::ReportFault.is_available_for_connector(&evse.connectors[0]));
    }

    #[test]
    fn clear_fault_restores_a_faulted_connector_to_available() {
        let mut evse = evse_with_statuses(&[ConnectorStatus::Faulted]);
        Command::ClearFault.apply_to(&mut evse, 0, "").unwrap();
        assert_eq!(evse.connectors[0].status, ConnectorStatus::Available);
    }

    #[test]
    fn clear_fault_is_unavailable_for_a_connector_that_is_not_faulted() {
        let evse = evse_with_statuses(&[ConnectorStatus::Available]);
        assert!(!Command::ClearFault.is_available_for_connector(&evse.connectors[0]));
    }

    #[test]
    fn only_commands_that_need_extra_input_report_a_parameter() {
        assert_eq!(
            Command::PlugInVehicle.parameter(),
            Some(CommandParameter::VehicleId)
        );
        assert_eq!(
            Command::PresentRfid.parameter(),
            Some(CommandParameter::RfidTag)
        );
        assert_eq!(
            Command::ReportFault.parameter(),
            Some(CommandParameter::FaultCode)
        );
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
        let message = Command::SetDisplayMessage
            .apply_to_charger(&mut charger, "Welcome to Flowion")
            .unwrap();

        assert_eq!(
            charger.display_message,
            Some("Welcome to Flowion".to_string())
        );
        assert!(message.contains("Welcome to Flowion"));
    }

    #[test]
    fn set_display_message_falls_back_to_a_default_when_left_blank() {
        let mut charger = charger_with_display(true);
        Command::SetDisplayMessage
            .apply_to_charger(&mut charger, "  ")
            .unwrap();

        assert_eq!(charger.display_message, Some("Welcome".to_string()));
    }

    #[test]
    fn clear_display_message_blanks_the_message() {
        let mut charger = charger_with_display(true);
        charger.display_message = Some("hello".to_string());

        Command::ClearDisplayMessage
            .apply_to_charger(&mut charger, "")
            .unwrap();

        assert_eq!(charger.display_message, None);
    }

    #[test]
    fn display_commands_do_nothing_on_a_charger_without_a_display() {
        let mut charger = charger_with_display(false);
        let result = Command::SetDisplayMessage.apply_to_charger(&mut charger, "hi");

        assert_eq!(result, None);
        assert_eq!(charger.display_message, None);
    }

    #[test]
    fn is_available_for_connector_checks_one_connector_in_isolation() {
        let evse = evse_with_statuses(&[ConnectorStatus::Available, ConnectorStatus::Occupied]);

        assert!(Command::PlugInVehicle.is_available_for_connector(&evse.connectors[0]));
        assert!(!Command::PlugInVehicle.is_available_for_connector(&evse.connectors[1]));
        assert!(Command::PresentRfid.is_available_for_connector(&evse.connectors[1]));
        assert!(!Command::PresentRfid.is_available_for_connector(&evse.connectors[0]));
    }

    #[test]
    fn apply_to_acts_on_the_given_connector_even_when_an_earlier_one_would_also_be_eligible() {
        let mut evse =
            evse_with_statuses(&[ConnectorStatus::Available, ConnectorStatus::Available]);

        let message = Command::PlugInVehicle
            .apply_to(&mut evse, 1, "MY-EV")
            .unwrap();

        assert_eq!(evse.connectors[0].status, ConnectorStatus::Available);
        assert!(evse.connectors[0].vehicle.is_none());
        assert_eq!(evse.connectors[1].status, ConnectorStatus::Occupied);
        assert_eq!(evse.connectors[1].vehicle.as_ref().unwrap().id, "MY-EV");
        assert!(message.contains("connector 2"));
    }

    #[test]
    fn apply_to_returns_none_and_mutates_nothing_for_an_ineligible_connector() {
        let mut evse = evse_with_statuses(&[ConnectorStatus::Available, ConnectorStatus::Occupied]);
        let before = evse.clone();

        let result = Command::PlugInVehicle.apply_to(&mut evse, 1, "");

        assert_eq!(result, None);
        assert_eq!(evse, before);
    }

    #[test]
    fn apply_to_returns_none_and_mutates_nothing_for_an_out_of_range_index() {
        let mut evse = evse_with_statuses(&[ConnectorStatus::Available]);
        let before = evse.clone();

        let result = Command::PlugInVehicle.apply_to(&mut evse, 5, "");

        assert_eq!(result, None);
        assert_eq!(evse, before);
    }

    #[test]
    fn every_parameter_placeholder_is_an_example_not_a_restatement_of_the_label() {
        for parameter in [
            CommandParameter::VehicleId,
            CommandParameter::RfidTag,
            CommandParameter::FaultCode,
            CommandParameter::DisplayMessage,
        ] {
            let placeholder = parameter.placeholder();
            assert!(!placeholder.is_empty(), "{parameter:?}");
            assert_ne!(placeholder, parameter.label(), "{parameter:?}");
        }
    }

    #[test]
    fn every_command_has_a_non_empty_description_distinct_from_its_label() {
        for command in Command::ALL {
            let description = command.description();
            assert!(
                !description.is_empty(),
                "{:?} has an empty description",
                command
            );
            assert!(
                description.len() <= 50,
                "{:?} description is too long for a palette row: {:?}",
                command,
                description
            );
            assert_ne!(
                description,
                command.label(),
                "{:?} description just repeats its label",
                command
            );
        }
    }
}
