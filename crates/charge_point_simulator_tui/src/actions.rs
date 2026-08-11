//! Actions the command palette can dispatch: `charge_point_simulator_core`'s [`Command`]s, which
//! go to the charger's OCPP state machine, and this crate's own [`HardwareAction`]s, which go
//! straight to its simulated hardware.
//!
//! The split is not cosmetic. A `Command` becomes a `ChargePointEvent`, so the charger's own state
//! machine decides what happens and the dashboard learns the result from the snapshot that comes
//! back. A `HardwareAction` has no protocol path at all - either because OCPP cannot express it
//! (nothing in `HardwareCommand` carries a power direction) or because the functional block that
//! would drive it is only registered when a CSMS exists to report to (`firmware_updates` and
//! `log_uploads`, see `core`'s `register_optional_hardware`). Keeping them different types, in
//! different channels (see [`crate::app::HardwareControl`]), is what stops the second kind from
//! reading as something the protocol did.
//!
//! Both appear in one palette regardless, because "what can I do to this charger right now" is one
//! question to the person asking it.

use charge_point_simulator_core::charger::{
    ChargerState, Command, ConnectorState, ConnectorStatus, FirmwareInstallStage,
};

use crate::app::CampaignProgress;

/// A direct action on the charger's simulated hardware, offered in the palette alongside the
/// protocol commands - see this module's own docs for why it is a separate type.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum HardwareAction {
    /// Flip the focused connector between exporting (V2G) and importing.
    ToggleDischarge,
    /// Fetch a firmware image and install it, with no CSMS campaign behind either half.
    InstallFirmware,
    /// Render the charger's diagnostics log and upload it, likewise with no CSMS behind it.
    UploadDiagnostics,
    /// Arm the installer to fail, so a CSMS's `InstallationFailed` handling can be exercised.
    FailFirmwareInstall,
    /// Arm the download half of the file transfer to fail (`DownloadFailed`).
    FailFirmwareDownload,
    /// Arm the upload half to fail (`UploadFailure`), independently of the download.
    FailDiagnosticsUpload,
}

impl HardwareAction {
    pub const ALL: [HardwareAction; 6] = [
        HardwareAction::ToggleDischarge,
        HardwareAction::InstallFirmware,
        HardwareAction::UploadDiagnostics,
        HardwareAction::FailFirmwareInstall,
        HardwareAction::FailFirmwareDownload,
        HardwareAction::FailDiagnosticsUpload,
    ];

    pub fn label(&self) -> &'static str {
        match self {
            HardwareAction::ToggleDischarge => "Toggle V2G discharge",
            HardwareAction::InstallFirmware => "Install firmware locally",
            HardwareAction::UploadDiagnostics => "Upload diagnostics locally",
            HardwareAction::FailFirmwareInstall => "Fail firmware installs",
            HardwareAction::FailFirmwareDownload => "Fail firmware downloads",
            HardwareAction::FailDiagnosticsUpload => "Fail diagnostics uploads",
        }
    }

    /// One short sentence saying what this does. Same length budget as [`Command::description`], so
    /// both kinds of row fit the palette identically.
    ///
    /// The three failure actions say "from now on" rather than "next", because that is what the
    /// hardware does: the fakes' failure flags are armed once and never cleared (see
    /// `FakeFirmwareInstaller::trigger_failure`). Calling it "fail the next one" would be a nicer
    /// sentence about behavior the simulator doesn't have.
    pub fn description(&self) -> &'static str {
        match self {
            HardwareAction::ToggleDischarge => "Exports power from the vehicle, or stops",
            HardwareAction::InstallFirmware => "Downloads and installs, with no CSMS",
            HardwareAction::UploadDiagnostics => "Uploads a log archive, with no CSMS",
            HardwareAction::FailFirmwareInstall => "Installs fail from now on",
            HardwareAction::FailFirmwareDownload => "Downloads fail from now on",
            HardwareAction::FailDiagnosticsUpload => "Uploads fail from now on",
        }
    }

    /// Whether this action targets one connector rather than the charger as a whole. Only discharge
    /// does; firmware and diagnostics are charger-wide, because the hardware behind them is (one
    /// installer, one file transfer per charger).
    pub fn is_connector_scoped(&self) -> bool {
        matches!(self, HardwareAction::ToggleDischarge)
    }

    /// Whether this action is offered right now.
    ///
    /// Every case is gated on the charger's **declared capabilities**, not on whether the hardware
    /// happens to exist: `App::charger_hardware` builds a piece only for a charger that declares the
    /// matching block, so the two agree - and a charger that told a CSMS under test it cannot do
    /// something must not then be able to do it from here.
    pub fn is_available(
        &self,
        charger: &ChargerState,
        connector: Option<&ConnectorState>,
        campaigns: &CampaignProgress,
    ) -> bool {
        let capabilities = &charger.config.capabilities;
        match self {
            HardwareAction::ToggleDischarge => {
                capabilities.supports_bidirectional_power
                    && connector.is_some_and(|connector| {
                        // Direction with nothing plugged in is a reading no real charger produces:
                        // the meter is gated on the contactor, which is gated on a session.
                        matches!(
                            connector.status,
                            ConnectorStatus::Occupied | ConnectorStatus::Charging
                        )
                    })
            }
            // One installer per charger tracks one installation, so offering a second while the
            // first is in flight would offer something the hardware cannot honour.
            HardwareAction::InstallFirmware => {
                capabilities.firmware_management
                    && campaigns.firmware_download.is_none()
                    && campaigns.firmware_install != Some(FirmwareInstallStage::Installing)
            }
            HardwareAction::UploadDiagnostics => {
                capabilities.diagnostics && campaigns.log_upload.is_none()
            }
            HardwareAction::FailFirmwareInstall => capabilities.firmware_management,
            // The file transfer backs both halves, and is built for either declaration - so a
            // download can be failed on a diagnostics-only charger, which is exactly where a CSMS
            // developer would want to fail one.
            HardwareAction::FailFirmwareDownload | HardwareAction::FailDiagnosticsUpload => {
                capabilities.firmware_management || capabilities.diagnostics
            }
        }
    }
}

/// One row of the command palette: a protocol command or a hardware action.
///
/// Ordered `Command` first in [`Self::label`]-independent ways (see `App::palette_entries`) only
/// because the protocol commands are what the palette has always led with; fuzzy matching then
/// reorders both kinds together, since a user typing "fw" does not care which kind of thing they
/// are reaching for.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum PaletteEntry {
    Command(Command),
    Hardware(HardwareAction),
}

impl PaletteEntry {
    pub fn label(&self) -> &'static str {
        match self {
            PaletteEntry::Command(command) => command.label(),
            PaletteEntry::Hardware(action) => action.label(),
        }
    }

    pub fn description(&self) -> &'static str {
        match self {
            PaletteEntry::Command(command) => command.description(),
            PaletteEntry::Hardware(action) => action.description(),
        }
    }

    /// Whether this entry acts on the focused connector, which is what decides whether the
    /// palette's target line names a connector or the charger.
    ///
    /// Display commands are charger-wide for the same reason firmware is: there is one display, and
    /// `Command::is_display_command` already draws that line.
    pub fn is_connector_scoped(&self) -> bool {
        match self {
            PaletteEntry::Command(command) => !command.is_display_command(),
            PaletteEntry::Hardware(action) => action.is_connector_scoped(),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use charge_point_simulator_core::charger::{
        CapabilitiesConfig, ChargerConfig, EvseConfig, OcppVersion,
    };

    fn charger(declare: impl FnOnce(&mut CapabilitiesConfig)) -> ChargerState {
        let mut capabilities = CapabilitiesConfig::default();
        declare(&mut capabilities);
        ChargerState::from_config(ChargerConfig {
            id: "CP001".into(),
            ocpp_version: OcppVersion::V21,
            evses: vec![EvseConfig {
                id: 1,
                connectors: 1,
            }],
            has_display: false,
            capabilities,
        })
    }

    #[test]
    fn a_charger_declaring_nothing_offers_no_hardware_actions_at_all() {
        let charger = charger(|_| {});
        let connector = &charger.evses[0].connectors[0];

        for action in HardwareAction::ALL {
            assert!(
                !action.is_available(&charger, Some(connector), &CampaignProgress::default()),
                "{action:?} should need a declaration"
            );
        }
    }

    #[test]
    fn discharge_needs_both_the_declaration_and_a_plugged_in_vehicle() {
        let mut charger = charger(|capabilities| capabilities.supports_bidirectional_power = true);
        let campaigns = CampaignProgress::default();

        let available = &charger.evses[0].connectors[0];
        assert!(!HardwareAction::ToggleDischarge.is_available(
            &charger,
            Some(available),
            &campaigns
        ));

        charger.evses[0].connectors[0].status = ConnectorStatus::Charging;
        let charging = &charger.evses[0].connectors[0];
        assert!(HardwareAction::ToggleDischarge.is_available(&charger, Some(charging), &campaigns));

        // No connector resolved (e.g. an EVSE with none) is not a licence to act on nothing.
        assert!(!HardwareAction::ToggleDischarge.is_available(&charger, None, &campaigns));
    }

    #[test]
    fn firmware_actions_need_firmware_management_and_diagnostics_needs_diagnostics() {
        let firmware = charger(|capabilities| capabilities.firmware_management = true);
        let diagnostics = charger(|capabilities| capabilities.diagnostics = true);
        let campaigns = CampaignProgress::default();

        assert!(HardwareAction::InstallFirmware.is_available(&firmware, None, &campaigns));
        assert!(!HardwareAction::InstallFirmware.is_available(&diagnostics, None, &campaigns));

        assert!(HardwareAction::UploadDiagnostics.is_available(&diagnostics, None, &campaigns));
        assert!(!HardwareAction::UploadDiagnostics.is_available(&firmware, None, &campaigns));

        // Both declarations bring the same file transfer, so either can fail either half.
        for charger in [&firmware, &diagnostics] {
            assert!(HardwareAction::FailFirmwareDownload.is_available(charger, None, &campaigns));
            assert!(HardwareAction::FailDiagnosticsUpload.is_available(charger, None, &campaigns));
        }
    }

    /// One installer tracks one installation, so a second must not be offered mid-campaign - nor
    /// while the image is still being fetched, since installing is the second half of that campaign.
    #[test]
    fn installing_firmware_is_not_offered_while_a_campaign_is_already_running() {
        let charger = charger(|capabilities| capabilities.firmware_management = true);

        let installing = CampaignProgress {
            firmware_install: Some(FirmwareInstallStage::Installing),
            ..Default::default()
        };
        assert!(!HardwareAction::InstallFirmware.is_available(&charger, None, &installing));

        let downloading = CampaignProgress {
            firmware_download: Some(charge_point_simulator_core::charger::InFlightTransfer {
                elapsed: std::time::Duration::ZERO,
                duration: std::time::Duration::from_secs(20),
                transferred_bytes: 0,
                total_bytes: 1024,
            }),
            ..Default::default()
        };
        assert!(!HardwareAction::InstallFirmware.is_available(&charger, None, &downloading));

        // An install that has *finished* is no bar to starting another.
        let installed = CampaignProgress {
            firmware_install: Some(FirmwareInstallStage::Installed),
            ..Default::default()
        };
        assert!(HardwareAction::InstallFirmware.is_available(&charger, None, &installed));
    }

    #[test]
    fn only_discharge_and_the_connector_commands_are_connector_scoped() {
        assert!(PaletteEntry::Hardware(HardwareAction::ToggleDischarge).is_connector_scoped());
        assert!(!PaletteEntry::Hardware(HardwareAction::InstallFirmware).is_connector_scoped());
        assert!(PaletteEntry::Command(Command::PlugInVehicle).is_connector_scoped());
        assert!(!PaletteEntry::Command(Command::SetDisplayMessage).is_connector_scoped());
    }

    #[test]
    fn every_action_has_a_description_that_fits_a_palette_row_and_says_something_new() {
        for action in HardwareAction::ALL {
            let description = action.description();
            assert!(!description.is_empty(), "{action:?}");
            assert!(
                description.len() <= 50,
                "{action:?} description is too long for a palette row: {description:?}"
            );
            assert_ne!(description, action.label(), "{action:?}");
        }
    }
}
